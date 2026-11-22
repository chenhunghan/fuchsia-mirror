// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <endian.h>
#include <fidl/fuchsia.driver.framework/cpp/wire.h>
#include <fidl/fuchsia.driver.token/cpp/wire.h>
#include <fidl/fuchsia.hardware.network/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.peripheral/cpp/wire.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async-loop/testing/cpp/real_loop.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/component/incoming/cpp/service_member_watcher.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/watcher.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/hid/boot.h>
#include <lib/usb-virtual-bus-launcher/usb-virtual-bus-launcher.h>
#include <lib/zx/fifo.h>
#include <lib/zx/vmo.h>
#include <unistd.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <algorithm>
#include <string>
#include <string_view>
#include <vector>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>
#include <usb/cdc.h>
#include <usb/descriptors.h>

#include "src/connectivity/lib/network-device/cpp/network_device_client.h"
#include "src/lib/testing/predicates/status.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_virtual_bus {
namespace {

using usb_virtual::BusLauncher;

constexpr const char kManufacturer[] = "Google";
constexpr const char kProduct[] = "CDC Ethernet";
constexpr const char kSerial[] = "ebfd5ad49d2a";
constexpr size_t kEthernetMtu = 1500;

class NetworkDeviceInterface {
 public:
  NetworkDeviceInterface() : loop_(&kAsyncLoopConfigNoAttachToCurrentThread) {}

  // Must be called exactly once, before any other method. Fatal assertions cannot be used in a
  // constructor with gtest, so initialization lives here instead; callers should wrap this in
  // ASSERT_NO_FATAL_FAILURE.
  void Init(fidl::ClientEnd<fuchsia_hardware_network::Device> device,
            const std::string& session_name) {
    netdevice_client_.emplace(std::move(device), loop_.dispatcher());
    network::client::NetworkDeviceClient& client = netdevice_client_.value();
    {
      std::optional<zx_status_t> result;
      client.OpenSession(session_name, [&result](zx_status_t status) { result = status; });
      RunLoopUntil([&result] { return result.has_value(); });
      ASSERT_OK(result.value());
    }
    ASSERT_TRUE(client.HasSession());
    client.SetRxCallback([&](network::client::NetworkDeviceClient::Buffer buf) {
      rx_queue_.emplace(std::move(buf));
    });
    {
      client.GetPorts(
          [this](zx::result<std::vector<network::client::netdev::wire::PortId>> ports_status) {
            ASSERT_OK(ports_status.status_value());
            std::vector<network::client::netdev::wire::PortId> ports =
                std::move(ports_status.value());
            ASSERT_EQ(ports.size(), 1u);
            port_id_ = ports[0];
          });
      RunLoopUntil([this] { return port_id_.has_value(); });
    }
    {
      std::optional<zx_status_t> result;
      client.AttachPort(port_id_.value(), {fuchsia_hardware_network::wire::FrameType::kEthernet},
                        [&result](zx_status_t status) { result = status; });
      RunLoopUntil([&result] { return result.has_value(); });
      ASSERT_OK(result.value());
    }

    network::client::DeviceInfo device_info = client.device_info();
    tx_depth_ = device_info.tx_depth;
    rx_depth_ = device_info.rx_depth;

    {
      bool checked_mtu = false;
      zx::result<std::unique_ptr<network::client::NetworkDeviceClient::StatusWatchHandle>> result =
          client.WatchStatus(
              port_id_.value(),
              [this, &checked_mtu](fuchsia_hardware_network::wire::PortStatus status) {
                if (status.has_mtu()) {
                  mtu_ = status.mtu();
                }
                checked_mtu = true;
              });
      RunLoopUntil([&checked_mtu] { return checked_mtu; });
      ASSERT_TRUE(mtu_.has_value());
    }
  }

  zx_status_t SendData(std::vector<uint8_t>& data) {
    network::client::NetworkDeviceClient::Buffer buffer = netdevice_client_.value().AllocTx();
    if (!buffer.is_valid()) {
      return ZX_ERR_NO_MEMORY;
    }

    // Populate the buffer data and metadata
    buffer.data().SetFrameType(fuchsia_hardware_network::wire::FrameType::kEthernet);
    buffer.data().SetPortId(port_id_.value());
    EXPECT_EQ(buffer.data().Write(data.data(), data.size()), data.size());
    zx_status_t status = buffer.Send();

    // Run the loop to give the Netdevice client an opportunity to send.
    RunLoopUntilIdle();

    return status;
  }

  zx::result<std::vector<uint8_t>> ReceiveData() {
    // Wait for the read callback registered with the Netdevice client to fill the queue.
    RunLoopUntil([this] { return !rx_queue_.empty(); });

    network::client::NetworkDeviceClient::Buffer buffer = std::move(rx_queue_.front());
    rx_queue_.pop();
    if (!buffer.is_valid()) {
      return zx::error(ZX_ERR_IO_DATA_INTEGRITY);
    }
    // Check that the port ID and frame type match what we expect.
    if ((buffer.data().port_id().base != port_id_.value().base) ||
        (buffer.data().port_id().salt != port_id_.value().salt) ||
        (buffer.data().frame_type() != fuchsia_hardware_network::wire::FrameType::kEthernet)) {
      ADD_FAILURE() << "Frame metadata does not match."
                    << " Received frame port ID base: "
                    << static_cast<uint16_t>(buffer.data().port_id().base)
                    << " Expected base: " << static_cast<uint16_t>(port_id_.value().base)
                    << " Received frame port ID salt: "
                    << static_cast<uint16_t>(buffer.data().port_id().salt)
                    << " Expected salt: " << static_cast<uint16_t>(port_id_.value().salt)
                    << " Received frame type: "
                    << static_cast<uint16_t>(static_cast<uint8_t>(buffer.data().frame_type()))
                    << " Expected frame type: "
                    << static_cast<uint16_t>(static_cast<uint8_t>(
                           fuchsia_hardware_network::wire::FrameType::kEthernet));
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    std::vector<uint8_t> output;
    output.resize(buffer.data().len());
    size_t read = buffer.data().Read(output.data(), output.size());
    if (read != output.size()) {
      return zx::error(ZX_ERR_INTERNAL);
    }
    return zx::ok(std::move(output));
  }

  uint32_t tx_depth() const { return tx_depth_; }

  uint32_t rx_depth() const { return rx_depth_; }

  size_t mtu() { return mtu_.value(); }

 private:
  async::Loop loop_;
  std::optional<network::client::NetworkDeviceClient> netdevice_client_;
  // Since the client receives data in a callback, we need to store the frames we receive.
  std::queue<network::client::NetworkDeviceClient::Buffer> rx_queue_;

  std::optional<network::client::netdev::wire::PortId> port_id_;
  uint32_t tx_depth_ = 0;
  uint32_t rx_depth_ = 0;
  std::optional<size_t> mtu_;

  // TODO(https://fxbug.dev/42065375): remove this hand-rolled implementation (adapted
  // from //sdk/lib/async-loop/testing/real_loop.cc) once `loop_fixture::RealLoop`
  // supports configuration.
  void RunLoopUntil(fit::function<bool()> condition) {
    while (loop_.GetState() == ASYNC_LOOP_RUNNABLE) {
      if (condition()) {
        loop_.ResetQuit();
        return;
      }
      loop_.Run(zx::deadline_after(zx::msec(1)), true);
    }
    loop_.ResetQuit();
  }

  void RunLoopUntilIdle() {
    loop_.RunUntilIdle();
    loop_.ResetQuit();
  }
};

class UsbCdcEcmTest : public ::testing::Test {
 public:
  void SetUp() override {
    auto bus = BusLauncher::Create({
        fuchsia_component_test::Capability::WithService(
            fuchsia_component_test::Service{{.name = fuchsia_hardware_network::Service::Name}}),
    });
    ASSERT_OK(bus.status_value());
    bus_ = std::move(bus.value());

    ASSERT_NO_FATAL_FAILURE(InitUsbCdcEcm(&peripheral_device_, &host_device_));
  }

  void TearDown() override {
    ASSERT_OK(bus_->ClearPeripheralDeviceFunctions());
    ASSERT_OK(bus_->Disable());
  }

 protected:
  std::optional<BusLauncher> bus_;
  fidl::ClientEnd<fuchsia_hardware_network::Device> peripheral_device_;
  fidl::ClientEnd<fuchsia_hardware_network::Device> host_device_;

  void InitUsbCdcEcm(fidl::ClientEnd<fuchsia_hardware_network::Device>* peripheral_device,
                     fidl::ClientEnd<fuchsia_hardware_network::Device>* host_device) {
    namespace usb_peripheral = fuchsia_hardware_usb_peripheral;
    using ConfigurationDescriptor =
        ::fidl::VectorView<fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor>;
    usb_peripheral::wire::DeviceDescriptor device_desc = {};
    device_desc.bcd_usb = htole16(0x0200);
    device_desc.b_device_class = 0;
    device_desc.b_device_sub_class = 0;
    device_desc.b_device_protocol = 0;
    device_desc.b_max_packet_size0 = 64;
    device_desc.bcd_device = htole16(0x0100);
    device_desc.b_num_configurations = 2;

    device_desc.manufacturer = fidl::StringView(kManufacturer);
    device_desc.product = fidl::StringView(kProduct);
    device_desc.serial = fidl::StringView(kSerial);

    device_desc.id_vendor = htole16(0x0BDA);
    device_desc.id_product = htole16(0x8152);

    usb_peripheral::wire::FunctionDescriptor usb_cdc_ecm_function_desc = {
        .interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
        .interface_subclass = fidl::ToUnderlying(fdescriptor::CdcSubclass::kEthernet),
        .interface_protocol = 0,
    };

    std::vector<usb_peripheral::wire::FunctionDescriptor> function_descs;
    function_descs.push_back(usb_cdc_ecm_function_desc);
    std::vector<ConfigurationDescriptor> config_descs;
    config_descs.emplace_back(
        fidl::VectorView<usb_peripheral::wire::FunctionDescriptor>::FromExternal(function_descs));
    config_descs.emplace_back(
        fidl::VectorView<usb_peripheral::wire::FunctionDescriptor>::FromExternal(function_descs));

    ASSERT_OK(bus_->SetupPeripheralDevice(std::move(device_desc), std::move(config_descs)));

    zx::result bus_topology =
        component::ConnectAt<fuchsia_driver_token::NodeBusTopology>(bus_->GetExposedDir());
    ASSERT_OK(bus_topology.status_value());

    component::SyncServiceMemberWatcher<fuchsia_hardware_network::Service::Token> watcher(
        bus_->GetExposedDir());
    while (!peripheral_device->is_valid() || !host_device->is_valid()) {
      std::string instance_name;
      zx::result token_client = watcher.GetNextInstance(false, &instance_name);
      ASSERT_OK(token_client.status_value());

      fidl::WireResult token_res = fidl::WireCall(token_client.value())->Get();
      ASSERT_OK(token_res.status());
      ASSERT_TRUE(token_res->is_ok()) << zx_status_get_string(token_res->error_value());

      fidl::WireResult topo_res =
          fidl::WireCall(bus_topology.value())->Get(std::move(token_res->value()->token));
      ASSERT_OK(topo_res.status());
      ASSERT_TRUE(topo_res->is_ok()) << zx_status_get_string(topo_res->error_value());

      for (const auto& bus_info : topo_res->value()->path) {
        if (!bus_info.has_bus()) {
          continue;
        }
        if (bus_info.bus() == fuchsia_driver_framework::wire::BusType::kUsbPeripheral) {
          // Configuration 1 (`function-000`, address 0) is unconfigured; select configuration 2
          // (`function-001`, address 1).
          if (bus_info.has_address() && bus_info.address().is_int_value() &&
              bus_info.address().int_value() == 1) {
            zx::result device =
                component::ConnectAtMember<fuchsia_hardware_network::Service::Device>(
                    bus_->GetExposedDir(), instance_name);
            ASSERT_OK(device.status_value());
            *peripheral_device = std::move(device.value());
          }
          break;
        }
        if (bus_info.bus() == fuchsia_driver_framework::wire::BusType::kUsb) {
          zx::result device = component::ConnectAtMember<fuchsia_hardware_network::Service::Device>(
              bus_->GetExposedDir(), instance_name);
          ASSERT_OK(device.status_value());
          *host_device = std::move(device.value());
          break;
        }
      }
    }
  }
};

void TransmitAndReceive(NetworkDeviceInterface& a, NetworkDeviceInterface& b,
                        const uint32_t fifo_depth) {
  uint8_t fill_data = 0;
  for (size_t i = 0; i < fifo_depth; ++i) {
    std::vector<uint8_t> data;
    for (size_t j = 0; j < kEthernetMtu; ++j) {
      data.push_back(fill_data++);
    }
    ASSERT_OK(a.SendData(data));
  }
  size_t received_bytes = 0;
  uint8_t read_data = 0;
  while (received_bytes < fifo_depth * kEthernetMtu) {
    zx::result<std::vector<uint8_t>> received_data = b.ReceiveData();
    ASSERT_OK(received_data.status_value());
    const std::vector<uint8_t>& data = received_data.value();
    ASSERT_EQ(kEthernetMtu, data.size());
    received_bytes += data.size();
    for (const auto& b : data) {
      ASSERT_EQ(b, read_data++);
    }
  }
}

// Waits for each path segment in |path| to appear in |dir_fd|, opening intermediate segments as
// directories without connecting to any devfs protocol, controller, or device_topology service
// (connecting to device_topology on classless nodes triggers a devfs allowlist debug assertion).
zx_status_t WaitForTopologicalNode(int dir_fd, std::string_view path) {
  fbl::unique_fd owned_dir;
  int current_fd = dir_fd;
  while (!path.empty()) {
    const size_t slash = path.find('/');
    const std::string_view segment =
        (slash == std::string_view::npos) ? path : path.substr(0, slash);
    if (segment.empty()) {
      return ZX_ERR_INVALID_ARGS;
    }
    std::string target(segment);
    zx_status_t watch_status = fdio_watch_directory(
        current_fd,
        [](int, int event, const char* fn, void* cookie) {
          if (event != WATCH_EVENT_ADD_FILE) {
            return ZX_OK;
          }
          const auto* expected = static_cast<const std::string*>(cookie);
          return (*expected == fn) ? ZX_ERR_STOP : ZX_OK;
        },
        ZX_TIME_INFINITE, &target);
    if (watch_status != ZX_ERR_STOP) {
      return watch_status;
    }
    if (slash == std::string_view::npos) {
      return ZX_OK;
    }
    fbl::unique_fd next_dir;
    if (zx_status_t status = fdio_open3_fd_at(
            current_fd, target.c_str(),
            uint64_t{fuchsia_io::wire::kPermReadable | fuchsia_io::wire::Flags::kProtocolDirectory},
            next_dir.reset_and_get_address());
        status != ZX_OK) {
      return status;
    }
    owned_dir = std::move(next_dir);
    current_fd = owned_dir.get();
    path.remove_prefix(slash + 1);
  }
  return ZX_ERR_INVALID_ARGS;
}

// Binds the driver over the virtual bus, then unbinds it with the interrupt transfer queued.
// Uses a single USB configuration (without loading network-device) so at most one instance of each
// DriverBase2 DSO runs in driver-host-root during teardown (see https://fxbug.dev/317149053).
TEST(UsbCdcEcmBindTest, BindThenUnbind) {
  zx::result bus = BusLauncher::Create();
  ASSERT_OK(bus.status_value());

  namespace usb_peripheral = fuchsia_hardware_usb_peripheral;
  using ConfigurationDescriptor =
      ::fidl::VectorView<fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor>;

  usb_peripheral::wire::DeviceDescriptor device_desc = {};
  device_desc.bcd_usb = htole16(0x0200);
  device_desc.b_max_packet_size0 = 64;
  device_desc.bcd_device = htole16(0x0100);
  device_desc.b_num_configurations = 1;
  device_desc.manufacturer = fidl::StringView(kManufacturer);
  device_desc.product = fidl::StringView(kProduct);
  device_desc.serial = fidl::StringView(kSerial);
  device_desc.id_vendor = htole16(0x18D1);
  device_desc.id_product = htole16(0xA020);

  usb_peripheral::wire::FunctionDescriptor usb_cdc_ecm_function_desc = {
      .interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
      .interface_subclass = fidl::ToUnderlying(fdescriptor::CdcSubclass::kEthernet),
      .interface_protocol = 0,
  };
  std::vector<usb_peripheral::wire::FunctionDescriptor> function_descs = {
      usb_cdc_ecm_function_desc,
  };
  std::vector<ConfigurationDescriptor> config_descs = {
      ConfigurationDescriptor::FromExternal(function_descs),
  };

  ASSERT_OK(bus->SetupPeripheralDevice(std::move(device_desc), std::move(config_descs)));

  ASSERT_OK(WaitForTopologicalNode(bus->GetRootFd(),
                                   "usb-virtual-host/usb-bus/000/000/asc-000/usb-cdc-ecm"));
  ASSERT_OK(WaitForTopologicalNode(
      bus->GetRootFd(), "usb-virtual-device/usb-peripheral/function-000/usb-cdc-netdev"));

  ASSERT_OK(bus->ClearPeripheralDeviceFunctions());
  ASSERT_OK(bus->Disable());
}

// TODO(b/316176095): Re-enable test after ensuring it works with DFv2.
TEST_F(UsbCdcEcmTest, DISABLED_TransmitReceive) {
  NetworkDeviceInterface peripheral;
  ASSERT_NO_FATAL_FAILURE(
      peripheral.Init(std::move(peripheral_device_), "usb-cdc-function-peripheral"));
  NetworkDeviceInterface host;
  ASSERT_NO_FATAL_FAILURE(host.Init(std::move(host_device_), "usb-cdc-ecm-host"));

  ASSERT_EQ(peripheral.mtu(), kEthernetMtu);
  ASSERT_EQ(host.mtu(), kEthernetMtu);

  auto pick_transmit_depth = [](const NetworkDeviceInterface& sender,
                                const NetworkDeviceInterface& receiver) -> uint32_t {
    // NOTE: Netdevice core will double the number of device buffers to account
    // for in-flight buffers with the higher layers. This is encoding knowledge
    // at a distance here, but basically we don't want to send more than the
    // actual underlying device's depth, which makes the transfer racy/fallible.
    //
    // TODO(https://fxbug.dev/42067498): Improve this when we add a signal that we
    // can observe for buffer readiness.
    return std::min(sender.tx_depth(), receiver.rx_depth() / 2);
  };

  ASSERT_NO_FATAL_FAILURE(
      TransmitAndReceive(peripheral, host, pick_transmit_depth(peripheral, host)));
  ASSERT_NO_FATAL_FAILURE(
      TransmitAndReceive(host, peripheral, pick_transmit_depth(host, peripheral)));
}

}  // namespace
}  // namespace usb_virtual_bus
