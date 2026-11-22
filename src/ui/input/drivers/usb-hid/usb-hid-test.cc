// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/input/drivers/usb-hid/usb-hid.h"

#include <dirent.h>
#include <endian.h>
#include <fcntl.h>
#include <fidl/fuchsia.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.input/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.peripheral/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.virtual.bus/cpp/wire.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/component/incoming/cpp/directory.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/ddk/platform-defs.h>
#include <lib/device-watcher/cpp/device-watcher.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fdio/cpp/caller.h>
#include <lib/fdio/directory.h>
#include <lib/fit/function.h>
#include <lib/hid/boot.h>
#include <lib/usb-virtual-bus-launcher/usb-virtual-bus-launcher.h>
#include <sys/stat.h>
#include <unistd.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <algorithm>
#include <cstring>
#include <mutex>
#include <optional>
#include <vector>

#include <fbl/string.h>
#include <gtest/gtest.h>
#include <usb/usb.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"
#include "src/lib/testing/predicates/status.h"

namespace usb_virtual_bus {
namespace {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;
namespace fhidbus = fuchsia_hardware_hidbus;

using usb_virtual::BusLauncher;

class UsbHidTest : public ::testing::Test {
 public:
  void SetUp() override {
    zx::result bus = BusLauncher::Create();
    ASSERT_OK(bus);
    bus_ = std::move(bus.value());

    auto usb_hid_function_desc = GetConfigDescriptor();
    ASSERT_NO_FATAL_FAILURE(InitUsbHid(usb_hid_function_desc));

    fdio_cpp::UnownedFdioCaller caller(bus_->GetRootFd());
    zx::result controller =
        component::ConnectAt<fuchsia_hardware_input::Controller>(caller.directory(), devpath_);
    ASSERT_OK(controller);
    auto [device, server] = fidl::Endpoints<fuchsia_hardware_input::Device>::Create();

    fidl::OneWayStatus status = fidl::WireCall(controller.value())->OpenSession(std::move(server));
    ASSERT_OK(status.status());

    sync_client_ = fidl::WireSyncClient<fuchsia_hardware_input::Device>(std::move(device));
  }

  void TearDown() override {
    if (bus_.has_value()) {
      ASSERT_OK(bus_->ClearPeripheralDeviceFunctions());
      ASSERT_OK(bus_->Disable());
    }
  }

 protected:
  virtual fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor GetConfigDescriptor() = 0;

  // Initialize a Usb HID device. Asserts on failure.
  void InitUsbHid(fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor desc) {
    namespace usb_peripheral = fuchsia_hardware_usb_peripheral;
    std::vector<usb_peripheral::wire::FunctionDescriptor> function_descs = {desc};
    ASSERT_OK(bus_->SetupPeripheralDevice(
        {
            .bcd_usb = htole16(0x0200),
            .b_max_packet_size0 = 64,
            .id_vendor = htole16(0x18d1),
            .id_product = htole16(0xaf10),
            .bcd_device = htole16(0x0100),
            .b_num_configurations = 1,
        },
        {fidl::VectorView<usb_peripheral::wire::FunctionDescriptor>::FromExternal(
            function_descs)}));
    fdio_cpp::UnownedFdioCaller caller(bus_->GetRootFd());
    {
      zx::result directory = component::OpenDirectoryAt(caller.directory(), "class/input");
      ASSERT_OK(directory);
      zx::result watch_result = device_watcher::WatchDirectoryForItems(
          directory.value(), [this](std::string_view devpath) {
            devpath_ = fbl::String::Concat({"class/input/", devpath});
            return std::monostate{};
          });
      ASSERT_OK(watch_result);
    }
  }

  std::optional<BusLauncher> bus_;
  fbl::String devpath_;
  fidl::WireSyncClient<fuchsia_hardware_input::Device> sync_client_;
};

class UsbOneEndpointTest : public UsbHidTest {
 protected:
  fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor GetConfigDescriptor() override {
    return fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor{
        .interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kHid),
        .interface_subclass = 0,
        .interface_protocol =
            fidl::ToUnderlying(fdescriptor::TestVendorProtocol::kTestHidOneEndpoint),
    };
  }
};

TEST_F(UsbOneEndpointTest, GetDeviceIdsVidPid) {
  // Check USB device descriptor VID/PID plumbing.
  auto result = sync_client_->Query();
  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_ok());
  EXPECT_EQ(0x18d1u, result.value()->info.vendor_id());
  EXPECT_EQ(0xaf10u, result.value()->info.product_id());
}

TEST_F(UsbOneEndpointTest, SetAndGetReport) {
  uint8_t buf[sizeof(hid_boot_mouse_report_t)] = {0xab, 0xbc, 0xde};

  auto set_result = sync_client_->SetReport(fhidbus::wire::ReportType::kInput, 0,
                                            fidl::VectorView<uint8_t>::FromExternal(buf));
  auto get_result = sync_client_->GetReport(fhidbus::wire::ReportType::kInput, 0);

  ASSERT_TRUE(set_result.ok());
  ASSERT_TRUE(set_result->is_ok());

  ASSERT_TRUE(get_result.ok());
  ASSERT_TRUE(get_result->is_ok());

  ASSERT_EQ(get_result.value()->report.size(), sizeof(hid_boot_mouse_report_t));
  ASSERT_EQ(0xab, get_result.value()->report[0]);
  ASSERT_EQ(0xbc, get_result.value()->report[1]);
  ASSERT_EQ(0xde, get_result.value()->report[2]);
}

class UsbTwoEndpointTest : public UsbHidTest {
 protected:
  fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor GetConfigDescriptor() override {
    return fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor{
        .interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kHid),
        .interface_subclass = 0,
        .interface_protocol =
            fidl::ToUnderlying(fdescriptor::TestVendorProtocol::kTestHidTwoEndpoint),
    };
  }
};

TEST_F(UsbTwoEndpointTest, SetAndGetReport) {
  uint8_t buf[sizeof(hid_boot_mouse_report_t)] = {0xab, 0xbc, 0xde};

  auto set_result = sync_client_->SetReport(fhidbus::wire::ReportType::kInput, 0,
                                            fidl::VectorView<uint8_t>::FromExternal(buf));
  auto get_result = sync_client_->GetReport(fhidbus::wire::ReportType::kInput, 0);

  ASSERT_TRUE(set_result.ok());
  ASSERT_TRUE(set_result->is_ok());

  ASSERT_TRUE(get_result.ok());
  ASSERT_TRUE(get_result->is_ok());

  ASSERT_EQ(get_result.value()->report.size(), sizeof(hid_boot_mouse_report_t));
  ASSERT_EQ(0xab, get_result.value()->report[0]);
  ASSERT_EQ(0xbc, get_result.value()->report[1]);
  ASSERT_EQ(0xde, get_result.value()->report[2]);
}

struct HidShortItemU8 {
  uint8_t b_prefix;
  uint8_t data;
} __PACKED;

struct HidShortItemU16 {
  uint8_t b_prefix;
  uint16_t data;
} __PACKED;

struct HidShortItemU32 {
  uint8_t b_prefix;
  uint32_t data;
} __PACKED;

template <typename T>
cpp20::span<const uint8_t> AsBytes(const T& descriptor) {
  return cpp20::span<const uint8_t>(reinterpret_cast<const uint8_t*>(&descriptor),
                                    sizeof(descriptor));
}

// Standard 34-byte FIDO Alliance U2F / CTAPHID report descriptor (Usage Page 0xF1D0, Usage 0x01).
struct PackedFidoCtapHidReportDescriptor {
  HidShortItemU16 usage_page;
  HidShortItemU8 usage;
  HidShortItemU8 collection_application;
  HidShortItemU8 input_usage;
  HidShortItemU8 input_logical_minimum;
  HidShortItemU16 input_logical_maximum;
  HidShortItemU8 input_report_size;
  HidShortItemU8 input_report_count;
  HidShortItemU8 input;
  HidShortItemU8 output_usage;
  HidShortItemU8 output_logical_minimum;
  HidShortItemU16 output_logical_maximum;
  HidShortItemU8 output_report_size;
  HidShortItemU8 output_report_count;
  HidShortItemU8 output;
  uint8_t end_collection;
} __PACKED;

const PackedFidoCtapHidReportDescriptor kFidoCtapHidReportDesc = {
    .usage_page = {.b_prefix = 0x06, .data = htole16(0xf1d0)},   // Usage Page (FIDO, 0xF1D0)
    .usage = {.b_prefix = 0x09, .data = 0x01},                   // Usage (U2F Authenticator)
    .collection_application = {.b_prefix = 0xa1, .data = 0x01},  // Collection (Application)
    .input_usage = {.b_prefix = 0x09, .data = 0x20},             //   Usage (Input Report Data)
    .input_logical_minimum = {.b_prefix = 0x15, .data = 0x00},   //   Logical Minimum (0)
    .input_logical_maximum = {.b_prefix = 0x26, .data = htole16(0x00ff)},  //   Logical Max (255)
    .input_report_size = {.b_prefix = 0x75, .data = 0x08},                 //   Report Size (8)
    .input_report_count = {.b_prefix = 0x95, .data = 0x40},                //   Report Count (64)
    .input = {.b_prefix = 0x81, .data = 0x02},                             //   Input (Data,Var,Abs)
    .output_usage = {.b_prefix = 0x09, .data = 0x21},                      //   Usage (Output Data)
    .output_logical_minimum = {.b_prefix = 0x15, .data = 0x00},            //   Logical Minimum (0)
    .output_logical_maximum = {.b_prefix = 0x26, .data = htole16(0x00ff)},  //   Logical Max (255)
    .output_report_size = {.b_prefix = 0x75, .data = 0x08},                 //   Report Size (8)
    .output_report_count = {.b_prefix = 0x95, .data = 0x40},                //   Report Count (64)
    .output = {.b_prefix = 0x91, .data = 0x02},  //   Output (Data,Var,Abs)
    .end_collection = 0xc0,                      // End Collection
};
static_assert(sizeof(kFidoCtapHidReportDesc) == 34);

struct PackedBootMouseReportDescriptor {
  HidShortItemU8 usage_page_generic_desktop;
  HidShortItemU8 usage_mouse;
  HidShortItemU8 collection_application;
  HidShortItemU8 usage_pointer;
  HidShortItemU8 collection_physical;
  HidShortItemU8 usage_page_button;
  HidShortItemU8 usage_minimum;
  HidShortItemU8 usage_maximum;
  HidShortItemU8 button_logical_minimum;
  HidShortItemU8 button_logical_maximum;
  HidShortItemU8 button_report_count;
  HidShortItemU8 button_report_size;
  HidShortItemU8 button_input;
  HidShortItemU8 padding_report_count;
  HidShortItemU8 padding_report_size;
  HidShortItemU8 padding_input;
  HidShortItemU8 axis_usage_page_generic_desktop;
  HidShortItemU8 usage_x;
  HidShortItemU8 usage_y;
  HidShortItemU8 axis_logical_minimum;
  HidShortItemU8 axis_logical_maximum;
  HidShortItemU8 axis_report_size;
  HidShortItemU8 axis_report_count;
  HidShortItemU8 axis_input;
  uint8_t end_collection_physical;
  uint8_t end_collection_application;
} __PACKED;

constexpr PackedBootMouseReportDescriptor kBootMouseReportDesc = {
    .usage_page_generic_desktop = {.b_prefix = 0x05, .data = 0x01},  // Usage Page (Generic Desktop)
    .usage_mouse = {.b_prefix = 0x09, .data = 0x02},                 // Usage (Mouse)
    .collection_application = {.b_prefix = 0xa1, .data = 0x01},      // Collection (Application)
    .usage_pointer = {.b_prefix = 0x09, .data = 0x01},               //   Usage (Pointer)
    .collection_physical = {.b_prefix = 0xa1, .data = 0x00},         //   Collection (Physical)
    .usage_page_button = {.b_prefix = 0x05, .data = 0x09},           //     Usage Page (Button)
    .usage_minimum = {.b_prefix = 0x19, .data = 0x01},               //     Usage Minimum (0x01)
    .usage_maximum = {.b_prefix = 0x29, .data = 0x03},               //     Usage Maximum (0x03)
    .button_logical_minimum = {.b_prefix = 0x15, .data = 0x00},      //     Logical Minimum (0)
    .button_logical_maximum = {.b_prefix = 0x25, .data = 0x01},      //     Logical Maximum (1)
    .button_report_count = {.b_prefix = 0x95, .data = 0x03},         //     Report Count (3)
    .button_report_size = {.b_prefix = 0x75, .data = 0x01},          //     Report Size (1)
    .button_input = {.b_prefix = 0x81, .data = 0x02},                //     Input (Data,Var,Abs)
    .padding_report_count = {.b_prefix = 0x95, .data = 0x01},        //     Report Count (1)
    .padding_report_size = {.b_prefix = 0x75, .data = 0x05},         //     Report Size (5)
    .padding_input = {.b_prefix = 0x81, .data = 0x03},               //     Input (Const,Var,Abs)
    .axis_usage_page_generic_desktop = {.b_prefix = 0x05,
                                        .data = 0x01},         //     Usage Page (Generic Desktop)
    .usage_x = {.b_prefix = 0x09, .data = 0x30},               //     Usage (X)
    .usage_y = {.b_prefix = 0x09, .data = 0x31},               //     Usage (Y)
    .axis_logical_minimum = {.b_prefix = 0x15, .data = 0x81},  //     Logical Minimum (-127)
    .axis_logical_maximum = {.b_prefix = 0x25, .data = 0x7f},  //     Logical Maximum (127)
    .axis_report_size = {.b_prefix = 0x75, .data = 0x08},      //     Report Size (8)
    .axis_report_count = {.b_prefix = 0x95, .data = 0x02},     //     Report Count (2)
    .axis_input = {.b_prefix = 0x81, .data = 0x06},            //     Input (Data,Var,Rel)
    .end_collection_physical = 0xc0,                           //   End Collection
    .end_collection_application = 0xc0,                        // End Collection
};
static_assert(sizeof(kBootMouseReportDesc) == 50);

class FakeUsbHidDevice : public ddk::UsbProtocol<FakeUsbHidDevice>,
                         public fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb::Usb> {
 public:
  static constexpr uint8_t kInterfaceNumber = 1;
  static constexpr uint8_t kEpInAddr = 0x81;
  static constexpr uint8_t kEpOutAddr = 0x02;

  FakeUsbHidDevice()
      : fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb::Usb>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher()) {
    SetReportDescriptor(AsBytes(kBootMouseReportDesc));
  }

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_USB};
    config.callbacks[ZX_PROTOCOL_USB] = usb_banjo_server_.callback();
    return config;
  }

  void SetReportDescriptor(cpp20::span<const uint8_t> report_desc,
                           std::optional<uint16_t> advertised_length = std::nullopt) {
    std::lock_guard<std::mutex> lock(mutex_);
    report_desc_.assign(report_desc.begin(), report_desc.end());
    const uint16_t hid_report_len =
        advertised_length.value_or(static_cast<uint16_t>(report_desc_.size()));

    struct PackedHidInterfaceDescriptors {
      usb_interface_descriptor_t interface;
      usb_endpoint_descriptor_t ep_in;
      usb_endpoint_descriptor_t ep_out;
      usb_hid_descriptor_t hid;
    } __PACKED;

    PackedHidInterfaceDescriptors packed = {
        .interface =
            {
                .b_length = sizeof(usb_interface_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
                .b_interface_number = kInterfaceNumber,
                .b_alternate_setting = 0,
                .b_num_endpoints = 2,
                .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kHid),
                .b_interface_sub_class = 0,
                .b_interface_protocol = 0,
                .i_interface = 0,
            },
        .ep_in =
            {
                .b_length = sizeof(usb_endpoint_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
                .b_endpoint_address = kEpInAddr,
                .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt),
                .w_max_packet_size = htole16(64),
                .b_interval = 4,
            },
        .ep_out =
            {
                .b_length = sizeof(usb_endpoint_descriptor_t),
                .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
                .b_endpoint_address = kEpOutAddr,
                .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt),
                .w_max_packet_size = htole16(64),
                .b_interval = 4,
            },
        .hid =
            {
                .bLength = sizeof(usb_hid_descriptor_t) + sizeof(usb_hid_descriptor_entry_t),
                .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kHid),
                .bcdHID = htole16(0x0111),
                .bCountryCode = 0,
                .bNumDescriptors = 1,
            },
    };
    const usb_hid_descriptor_entry_t hid_entry = {
        .bDescriptorType = static_cast<uint8_t>(fhidbus::wire::HidDescriptorType::kReport),
        .wDescriptorLength = htole16(hid_report_len),
    };

    interface_descriptors_.resize(sizeof(packed) + sizeof(hid_entry));
    memcpy(interface_descriptors_.data(), &packed, sizeof(packed));
    memcpy(interface_descriptors_.data() + sizeof(packed), &hid_entry, sizeof(hid_entry));
  }

  void SetReportDescriptorControlStatus(zx_status_t status) {
    std::lock_guard<std::mutex> lock(mutex_);
    report_desc_status_ = status;
  }

  std::optional<uint16_t> last_report_desc_w_index() {
    std::lock_guard<std::mutex> lock(mutex_);
    return last_report_desc_w_index_;
  }

  // ddk::UsbProtocol implementation.
  zx_status_t UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                            int64_t timeout, const uint8_t* write_buffer, size_t write_size) {
    return ZX_OK;
  }

  zx_status_t UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                           int64_t timeout, uint8_t* out_read_buffer, size_t read_size,
                           size_t* out_read_actual) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (request_type == kStandardInterfaceIn &&
        request == fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor) &&
        (value >> 8) == static_cast<uint16_t>(fhidbus::wire::HidDescriptorType::kReport)) {
      last_report_desc_w_index_ = index;
      if (report_desc_status_ != ZX_OK) {
        if (out_read_actual != nullptr) {
          *out_read_actual = 0;
        }
        return report_desc_status_;
      }
      const size_t to_copy = std::min(read_size, report_desc_.size());
      if (to_copy > 0) {
        memcpy(out_read_buffer, report_desc_.data(), to_copy);
      }
      if (out_read_actual != nullptr) {
        *out_read_actual = to_copy;
      }
      return ZX_OK;
    }
    return ZX_ERR_NOT_SUPPORTED;
  }

  void UsbRequestQueue(usb_request_t* usb_request,
                       const usb_request_complete_callback_t* complete_cb) {}
  usb_speed_t UsbGetSpeed() { return fidl::ToUnderlying(fdescriptor::UsbSpeed::kFull); }
  zx_status_t UsbSetInterface(uint8_t interface_number, uint8_t alt_setting) { return ZX_OK; }
  uint8_t UsbGetConfiguration() { return 1; }
  zx_status_t UsbSetConfiguration(uint8_t configuration) { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
    return ZX_OK;
  }
  zx_status_t UsbResetEndpoint(uint8_t ep_address) { return ZX_OK; }
  zx_status_t UsbResetDevice() { return ZX_ERR_NOT_SUPPORTED; }
  size_t UsbGetMaxTransferSize(uint8_t ep_address) { return 64; }
  uint32_t UsbGetDeviceId() { return 1; }
  void UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc) {
    *out_desc = usb_device_descriptor_t{
        .b_length = sizeof(usb_device_descriptor_t),
        .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice),
        .bcd_usb = htole16(0x0200),
        .b_max_packet_size0 = 64,
        .id_vendor = htole16(0x1050),
        .id_product = htole16(0x0407),
        .bcd_device = htole16(0x0500),
        .b_num_configurations = 1,
    };
  }
  zx_status_t UsbGetConfigurationDescriptorLength(uint8_t configuration, size_t* out_length) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbGetConfigurationDescriptor(uint8_t configuration, uint8_t* out_desc_buffer,
                                            size_t desc_size, size_t* out_desc_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  size_t UsbGetDescriptorsLength() {
    std::lock_guard<std::mutex> lock(mutex_);
    return interface_descriptors_.size();
  }
  void UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size, size_t* out_descs_actual) {
    std::lock_guard<std::mutex> lock(mutex_);
    const size_t to_copy = std::min(descs_size, interface_descriptors_.size());
    memcpy(out_descs_buffer, interface_descriptors_.data(), to_copy);
    *out_descs_actual = to_copy;
  }
  zx_status_t UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id, uint16_t* out_lang_id,
                                     uint8_t* out_string_buffer, size_t string_size,
                                     size_t* out_string_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  zx_status_t UsbCancelAll(uint8_t ep_address) { return ZX_OK; }
  uint64_t UsbGetCurrentFrame() { return 0; }
  size_t UsbGetRequestSize() { return sizeof(usb_request_t); }

 private:
  compat::BanjoServer usb_banjo_server_{ZX_PROTOCOL_USB, this, &usb_protocol_ops_};
  std::mutex mutex_;
  std::vector<uint8_t> interface_descriptors_ __TA_GUARDED(mutex_);
  std::vector<uint8_t> report_desc_ __TA_GUARDED(mutex_);
  zx_status_t report_desc_status_ __TA_GUARDED(mutex_) = ZX_OK;
  std::optional<uint16_t> last_report_desc_w_index_ __TA_GUARDED(mutex_);
};

class UsbHidTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    device_server_.Initialize(component::kDefaultInstance, std::nullopt,
                              fake_usb_.GetBanjoConfig());
    zx_status_t status = device_server_.Serve(dispatcher, &to_driver_vfs);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return to_driver_vfs.AddService<fuchsia_hardware_usb::UsbService>(
        fuchsia_hardware_usb::UsbService::InstanceHandler({
            .device = fake_usb_.bind_handler(dispatcher),
        }));
  }

  FakeUsbHidDevice& fake_usb() { return fake_usb_; }

 private:
  FakeUsbHidDevice fake_usb_;
  compat::DeviceServer device_server_;
};

struct UsbHidDriverTestConfig final {
  using DriverType = usb_hid::UsbHidbus;
  using EnvironmentType = UsbHidTestEnvironment;
};

class UsbHidDriverUnitTest : public ::testing::Test {
 public:
  void TearDown() override { ASSERT_OK(driver_test_.StopDriver()); }

 protected:
  fdf_testing::BackgroundDriverTest<UsbHidDriverTestConfig> driver_test_;
};

TEST(UsbHidReportDescriptorFilterTest, IdentifiesFidoAllianceUsagePage) {
  EXPECT_TRUE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(AsBytes(kFidoCtapHidReportDesc)));

  struct PackedFourByteUsagePageHeader {
    HidShortItemU32 usage_page;
    HidShortItemU8 usage;
  } __PACKED;
  const PackedFourByteUsagePageHeader kFourByteFidoUsagePage = {
      .usage_page = {.b_prefix = 0x07, .data = htole32(0x0000f1d0)},
      .usage = {.b_prefix = 0x09, .data = 0x01},
  };
  EXPECT_TRUE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(AsBytes(kFourByteFidoUsagePage)));

  EXPECT_FALSE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(AsBytes(kBootMouseReportDesc)));

  struct PackedTwoByteUsagePageHeader {
    HidShortItemU16 usage_page;
    HidShortItemU8 usage;
  } __PACKED;
  const PackedTwoByteUsagePageHeader kVendorDefinedPage = {
      .usage_page = {.b_prefix = 0x06, .data = htole16(0xff00)},
      .usage = {.b_prefix = 0x09, .data = 0x01},
  };
  EXPECT_FALSE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(AsBytes(kVendorDefinedPage)));

  EXPECT_FALSE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(
      AsBytes(kFidoCtapHidReportDesc).subspan(0, 2)));

  EXPECT_FALSE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(
      AsBytes(kFourByteFidoUsagePage).subspan(0, 4)));

  EXPECT_FALSE(usb_hid::UsbHidbus::IsFidoAllianceReportDescriptor(cpp20::span<const uint8_t>{}));
}

TEST_F(UsbHidDriverUnitTest, SkipsFidoAllianceCtapHidInterfaceWithoutClaimingEndpoints) {
  driver_test_.RunInEnvironmentTypeContext([](UsbHidTestEnvironment& env) {
    env.fake_usb().SetReportDescriptor(AsBytes(kFidoCtapHidReportDesc));
  });

  // Start() must return ZX_ERR_NOT_SUPPORTED without connecting to either interrupt endpoint
  // (FakeUsbFidlProvider asserts expected_connect_to_endpoint_ is non-empty if ConnectToEndpoint is
  // called unexpectedly).
  zx::result<> start_result = driver_test_.StartDriver();
  ASSERT_TRUE(start_result.is_error());
  EXPECT_EQ(start_result.status_value(), ZX_ERR_NOT_SUPPORTED);

  driver_test_.RunInEnvironmentTypeContext([](UsbHidTestEnvironment& env) {
    EXPECT_EQ(env.fake_usb().last_report_desc_w_index(), FakeUsbHidDevice::kInterfaceNumber);
  });
  driver_test_.RunInNodeContext(
      [](fdf_testing::TestNode& node) { EXPECT_TRUE(node.children().empty()); });
}

TEST_F(UsbHidDriverUnitTest, BindsStandardHidInterfaceAndPublishesHidbus) {
  driver_test_.RunInEnvironmentTypeContext([](UsbHidTestEnvironment& env) {
    env.fake_usb().SetReportDescriptor(AsBytes(kBootMouseReportDesc));
    env.fake_usb().ExpectConnectToEndpoint(FakeUsbHidDevice::kEpInAddr);
    env.fake_usb().ExpectConnectToEndpoint(FakeUsbHidDevice::kEpOutAddr);
  });

  ASSERT_OK(driver_test_.StartDriver());

  driver_test_.RunInNodeContext([](fdf_testing::TestNode& node) {
    EXPECT_TRUE(node.children().contains(std::string(usb_hid::UsbHidbus::kChildNodeName)));
  });

  zx::result hidbus_client = driver_test_.Connect<fhidbus::Service::Device>();
  ASSERT_OK(hidbus_client);
  fidl::WireResult query = fidl::WireCall(*hidbus_client)->Query();
  ASSERT_OK(query.status());
  ASSERT_TRUE(query->is_ok());
  EXPECT_EQ(query.value()->info.vendor_id(), 0x1050u);
  EXPECT_EQ(query.value()->info.product_id(), 0x0407u);
  EXPECT_EQ(query.value()->info.dev_num(), FakeUsbHidDevice::kInterfaceNumber);
}

TEST_F(UsbHidDriverUnitTest, BindsWhenReportDescriptorReadReturnsShortTransfer) {
  // Advertise a 34-byte report descriptor in the HID descriptor, but return only the 2-byte prefix
  // ({0x06, 0xd0}) from ControlIn so actual_len < 3.
  driver_test_.RunInEnvironmentTypeContext([](UsbHidTestEnvironment& env) {
    env.fake_usb().SetReportDescriptor(AsBytes(kFidoCtapHidReportDesc).subspan(0, 2),
                                       /*advertised_length=*/sizeof(kFidoCtapHidReportDesc));
    env.fake_usb().ExpectConnectToEndpoint(FakeUsbHidDevice::kEpInAddr);
    env.fake_usb().ExpectConnectToEndpoint(FakeUsbHidDevice::kEpOutAddr);
  });

  ASSERT_OK(driver_test_.StartDriver());
}

TEST_F(UsbHidDriverUnitTest, BindsWhenReportDescriptorFetchFails) {
  driver_test_.RunInEnvironmentTypeContext([](UsbHidTestEnvironment& env) {
    env.fake_usb().SetReportDescriptor(AsBytes(kFidoCtapHidReportDesc));
    env.fake_usb().SetReportDescriptorControlStatus(ZX_ERR_TIMED_OUT);
    env.fake_usb().ExpectConnectToEndpoint(FakeUsbHidDevice::kEpInAddr);
    env.fake_usb().ExpectConnectToEndpoint(FakeUsbHidDevice::kEpOutAddr);
  });

  ASSERT_OK(driver_test_.StartDriver());
}

}  // namespace
}  // namespace usb_virtual_bus
