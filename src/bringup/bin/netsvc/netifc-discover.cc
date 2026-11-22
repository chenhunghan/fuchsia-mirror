// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/netsvc/netifc-discover.h"

#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <fidl/fuchsia.driver.token/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/component/incoming/cpp/directory_watcher.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>
#include <stdio.h>

#include <memory>
#include <print>
#include <string>
#include <string_view>

#include "src/bringup/bin/netsvc/match.h"

namespace {

std::string_view SkipInstanceSigil(std::string_view v) {
  if (!v.empty() && v.at(0) == '@') {
    return v.substr(1);
  }
  return v;
}

struct Netdevice {
  struct Info {
    fidl::ClientEnd<fuchsia_hardware_network::Device> device;
    fidl::ClientEnd<fuchsia_hardware_network::PortWatcher> port_watcher;
  };

  static std::optional<Info> GetInterfaceFromDevice(
      fidl::ClientEnd<fuchsia_hardware_network::Device> client_end) {
    auto [watcher_client_end, watcher_server_end] =
        fidl::Endpoints<fuchsia_hardware_network::PortWatcher>::Create();

    {
      fidl::Status result =
          fidl::WireCall(client_end)->GetPortWatcher(std::move(watcher_server_end));
      if (!result.ok()) {
        std::println("netifc: failed to get port watcher: {}", result.status_string());
        return std::nullopt;
      }
    }

    return Info{
        .device = std::move(client_end),
        .port_watcher = std::move(watcher_client_end),
    };
  }

  using PortWatcherGuard = fit::deferred_action<fit::callback<void()>>;

  static void Process(std::optional<NetdeviceInterface>& discovered, async_dispatcher_t* dispatcher,
                      size_t& active_port_watchers, Info info) {
    auto [device, watcher] = std::move(info);
    std::shared_ptr client_ptr =
        std::make_shared<fidl::WireClient<fuchsia_hardware_network::PortWatcher>>(
            std::move(watcher), dispatcher);
    ++active_port_watchers;
    std::shared_ptr watcher_guard = std::make_shared<PortWatcherGuard>(
        fit::defer_callback([&active_port_watchers] { --active_port_watchers; }));

    Watch(discovered, client_ptr, std::move(watcher_guard), std::move(device));
  }

  static void Watch(
      std::optional<NetdeviceInterface>& discovered,
      const std::shared_ptr<fidl::WireClient<fuchsia_hardware_network::PortWatcher>>& watcher,
      std::shared_ptr<PortWatcherGuard> watcher_guard,
      fidl::ClientEnd<fuchsia_hardware_network::Device> dev) {
    (*watcher)->Watch().ThenExactlyOnce(
        [&discovered, watcher, watcher_guard = std::move(watcher_guard), dev = std::move(dev)](
            fidl::WireUnownedResult<fuchsia_hardware_network::PortWatcher::Watch>& r) mutable {
          if (!r.ok()) {
            std::println("netifc: failed to watch for netdevice ports: {}", r.FormatDescription());
            return;
          }

          auto defer = fit::defer([&discovered, &watcher, &watcher_guard, &dev] {
            // Watch next.
            Watch(discovered, watcher, std::move(watcher_guard), std::move(dev));
          });

          const fuchsia_hardware_network::wire::DevicePortEvent& event = r.value().event;
          fuchsia_hardware_network::wire::PortId port_id;
          switch (event.Which()) {
            case fuchsia_hardware_network::wire::DevicePortEvent::Tag::kAdded:
              port_id = event.added();
              break;
            case fuchsia_hardware_network::wire::DevicePortEvent::Tag::kExisting:
              port_id = event.existing();
              break;
            case fuchsia_hardware_network::wire::DevicePortEvent::Tag::kIdle:
            case fuchsia_hardware_network::wire::DevicePortEvent::Tag::kRemoved:
              return;
          }

          auto [port_client_end, port_server_end] =
              fidl::Endpoints<fuchsia_hardware_network::Port>::Create();
          {
            fidl::Status result = fidl::WireCall(dev)->GetPort(port_id, std::move(port_server_end));
            if (!result.ok()) {
              std::println("netifc: failed to get netdevice port ({}:{}): {}", port_id.base,
                           port_id.salt, result.FormatDescription());
              return;
            }
          }
          {
            fidl::WireResult result = fidl::WireCall(port_client_end)->GetInfo();
            if (!result.ok()) {
              std::println("netifc: failed to get netdevice port info ({}:{}): {}", port_id.base,
                           port_id.salt, result.FormatDescription());
              return;
            }
            const fuchsia_hardware_network::wire::PortInfo& port_info = result.value().info;
            if (!(port_info.has_base_info() && port_info.base_info().has_port_class())) {
              std::println("netifc: missing port class in netdevice port info ({}:{}): {}",
                           port_id.base, port_id.salt, result.FormatDescription());
              return;
            }
            switch (port_info.base_info().port_class()) {
              case fuchsia_hardware_network::wire::PortClass::kEthernet:
              case fuchsia_hardware_network::wire::PortClass::kVirtual:
                // NB: Historically this only netifc only accepts Ethernet
                // device. We allow virtual interfaces as well which are used in
                // testing.
                break;
              default:
                std::println("netifc: ignoring netdevice port ({}:{}) with class {}", port_id.base,
                             port_id.salt,
                             static_cast<unsigned short>(port_info.base_info().port_class()));
                return;
            }
          }

          // This is a good candidate port, but we need to retrieve the MAC
          // address.
          auto [mac_client_end, mac_server_end] =
              fidl::Endpoints<fuchsia_hardware_network::MacAddressing>::Create();
          {
            fidl::Status result =
                fidl::WireCall(port_client_end)->GetMac(std::move(mac_server_end));
            if (!result.ok()) {
              std::println("netifc: failed to get mac addressing for port ({}:{}): {}",
                           port_id.base, port_id.salt, result.FormatDescription());
              return;
            }
          }

          fidl::WireResult result = fidl::WireCall(mac_client_end)->GetUnicastAddress();
          if (!result.ok()) {
            std::println("netifc: failed to get mac address for port ({}:{}): {}", port_id.base,
                         port_id.salt, result.FormatDescription());
            return;
          }
          const fuchsia_net::wire::MacAddress& mac = result.value().address;

          // We have our device, store it and stop watching.
          NetdeviceInterface& discovered_interface = discovered.emplace(NetdeviceInterface{
              .device = std::move(dev),
              .port_id = port_id,
          });
          static_assert(sizeof(mac.octets) == sizeof(discovered_interface.mac.x));
          std::copy(mac.octets.begin(), mac.octets.end(), std::begin(discovered_interface.mac.x));
          defer.cancel();
        });
  }
};

void netifc_evaluate(std::string_view topological_path,
                     fidl::UnownedClientEnd<fuchsia_io::Directory> dir,
                     const std::string& instance_name,
                     std::optional<NetdeviceInterface>& discovered, async_dispatcher_t* dispatcher,
                     size_t& active_port_watchers) {
  // If an interface was specified, check the bus topology of this device and reject it if it
  // doesn't match.
  if (!topological_path.empty()) {
    std::string token_path = instance_name + "/" + fuchsia_hardware_network::Service::Token::Name;
    zx::result token_client =
        component::ConnectAt<fuchsia_driver_token::NodeToken>(dir, token_path);
    if (token_client.is_error()) {
      std::println("netifc: failed to connect to {}: {}", token_path, token_client.status_string());
      return;
    }
    fidl::WireResult token_res = fidl::WireCall(token_client.value())->Get();
    if (!token_res.ok()) {
      std::println("netifc: NodeToken::Get failed for {}: {}", instance_name,
                   token_res.status_string());
      return;
    }
    if (token_res->is_error()) {
      std::println("netifc: NodeToken::Get returned error for {}: {}", instance_name,
                   zx_status_get_string(token_res->error_value()));
      return;
    }

    zx::result bus_topo_client = component::Connect<fuchsia_driver_token::NodeBusTopology>();
    if (bus_topo_client.is_error()) {
      std::println("netifc: failed to connect to NodeBusTopology: {}",
                   bus_topo_client.status_string());
      return;
    }
    fidl::WireResult topo_res =
        fidl::WireCall(bus_topo_client.value())->Get(std::move(token_res->value()->token));
    if (!topo_res.ok()) {
      std::println("netifc: NodeBusTopology::Get failed for {}: {}", instance_name,
                   topo_res.status_string());
      return;
    }
    if (topo_res->is_error()) {
      std::println("netifc: NodeBusTopology::Get returned error for {}: {}", instance_name,
                   zx_status_get_string(topo_res->error_value()));
      return;
    }

    std::string bus_path = BusInfoToPath(topo_res->value()->path);

    // Allow for limited wildcard matching to avoid coupling too tightly.
    if (!EndsWithWildcardMatch(bus_path, topological_path)) {
      std::println("netifc: ? {} (instance {})", bus_path, instance_name);
      return;
    }
  }

  fidl::ClientEnd<fuchsia_hardware_network::Device> dev;
  {
    std::string device_path = instance_name + "/" + fuchsia_hardware_network::Service::Device::Name;
    zx::result status = component::ConnectAt<fuchsia_hardware_network::Device>(dir, device_path);
    if (status.is_error()) {
      std::println("netifc: failed to connect to {}: {}", device_path, status.status_string());
      return;
    }
    dev = std::move(status.value());
  }

  std::optional result = Netdevice::GetInterfaceFromDevice(std::move(dev));
  if (result.has_value()) {
    std::println("netsvc: using {}", instance_name);
    Netdevice::Process(discovered, dispatcher, active_port_watchers, std::move(result.value()));
  }
}

}  // namespace

std::string BusInfoToPath(fidl::VectorView<fuchsia_driver_framework::wire::BusInfo> bus_info_list) {
  std::string bus_path;
  for (const auto& bus_info : bus_info_list) {
    if (!bus_info.has_address()) {
      continue;
    }
    std::string segment;
    switch (bus_info.address().Which()) {
      case fuchsia_driver_framework::wire::DeviceAddress::Tag::kIntValue:
        segment = std::to_string(bus_info.address().int_value());
        break;
      case fuchsia_driver_framework::wire::DeviceAddress::Tag::kArrayIntValue:
        if (bus_info.has_bus() && bus_info.bus() == fuchsia_driver_framework::wire::BusType::kPci &&
            bus_info.address().array_int_value().size() == 3) {
          const auto& bdf = bus_info.address().array_int_value();
          char buf[10];
          // Match DFv2 PCI node naming (`%02x_%02x_%01x` in `pci::Device::node_name`).
          snprintf(buf, sizeof(buf), "%02x_%02x_%x", bdf[0], bdf[1], bdf[2]);
          segment = buf;
        } else {
          for (size_t i = 0; i < bus_info.address().array_int_value().size(); ++i) {
            if (i > 0) {
              segment += ':';
            }
            char buf[4];
            snprintf(buf, sizeof(buf), "%02x", bus_info.address().array_int_value()[i]);
            segment += buf;
          }
        }
        break;
      case fuchsia_driver_framework::wire::DeviceAddress::Tag::kCharIntValue:
        segment = std::string(bus_info.address().char_int_value().get());
        break;
      case fuchsia_driver_framework::wire::DeviceAddress::Tag::kArrayCharIntValue:
        for (size_t i = 0; i < bus_info.address().array_char_int_value().size(); ++i) {
          if (i > 0) {
            segment += ':';
          }
          segment += std::string(bus_info.address().array_char_int_value()[i].get());
        }
        break;
      case fuchsia_driver_framework::wire::DeviceAddress::Tag::kStringValue:
        segment = std::string(bus_info.address().string_value().get());
        break;
      default:
        break;
    }
    std::string_view trimmed = segment;
    while (trimmed.starts_with('/')) {
      trimmed.remove_prefix(1);
    }
    if (!trimmed.empty()) {
      bus_path += '/';
      bus_path += trimmed;
    }
  }
  return bus_path;
}

zx::result<NetdeviceInterface> netifc_discover(std::string_view topological_path) {
  topological_path = SkipInstanceSigil(topological_path);
  std::optional<NetdeviceInterface> selected_ifc;
  size_t active_port_watchers = 0;
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);

  const std::string kServiceDir = std::string("/svc/") + fuchsia_hardware_network::Service::Name;
  zx::result<fidl::ClientEnd<fuchsia_io::Directory>> svc_dir =
      component::OpenServiceRoot(kServiceDir);
  if (svc_dir.is_error()) {
    std::println("netsvc: OpenServiceRoot returned error for {}: {}", kServiceDir,
                 svc_dir.status_string());
    return zx::error(svc_dir.error_value());
  }

  component::DirectoryWatcher watcher;
  zx_status_t status = watcher.Begin(
      svc_dir.value().borrow(),
      [&selected_ifc, &loop, &svc_dir, &active_port_watchers, topological_path](
          fuchsia_io::wire::WatchEvent event, const std::string& filename) {
        if ((event == fuchsia_io::wire::WatchEvent::kAdded ||
             event == fuchsia_io::wire::WatchEvent::kExisting) &&
            filename != ".") {
          netifc_evaluate(topological_path, svc_dir.value().borrow(), filename, selected_ifc,
                          loop.dispatcher(), active_port_watchers);
        }
      },
      loop.dispatcher());

  if (status != ZX_OK) {
    std::println("netsvc: failed to create watcher for {}: {}", kServiceDir,
                 zx_status_get_string(status));
    return zx::error(status);
  }

  for (;;) {
    if (!watcher.is_watching() && active_port_watchers == 0) {
      std::println("netsvc: service directory closed with no interfaces found");
      return zx::error(ZX_ERR_PEER_CLOSED);
    }
    zx_status_t run_status = loop.Run(zx::time::infinite(), /* once */ true);
    if (selected_ifc.has_value()) {
      return zx::ok(std::move(selected_ifc.value()));
    }
    if (run_status != ZX_OK) {
      std::println("netsvc: run loop error: {}", zx_status_get_string(run_status));
      return zx::error(run_status);
    }
  }
}
