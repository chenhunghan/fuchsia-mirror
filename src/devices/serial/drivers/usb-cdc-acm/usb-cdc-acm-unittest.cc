// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-cdc-acm.h"

#include <endian.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_server.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/zx/time.h>
#include <string.h>
#include <zircon/compiler.h>

#include <algorithm>
#include <mutex>
#include <thread>
#include <utility>
#include <vector>

#include <gtest/gtest.h>
#include <usb/descriptors.h>
#include <usb/usb-request.h>
#include <usb/usb.h>

#include "src/lib/testing/predicates/status.h"

namespace usb_cdc_acm_serial {
namespace {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

constexpr zx::duration kEnvironmentPollingPeriod = zx::msec(50);

// Spans several write requests but fits in the first batch, so every request is queued directly
// by Write() and none is refilled from WriteComplete().
constexpr size_t kWriteSize = 2 * kUsbBufferSize + 904;
static_assert(kWriteSize > kUsbBufferSize);
static_assert(kWriteSize <= kWriteRequestCount * kUsbBufferSize);
constexpr size_t kExpectedWriteRequests = (kWriteSize + kUsbBufferSize - 1) / kUsbBufferSize;

constexpr uint8_t kBulkInAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn) | 1;
constexpr uint8_t kBulkOutAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut) | 2;

class FakeUsb : public ddk::UsbProtocol<FakeUsb> {
 public:
  struct QueuedRequest {
    usb_request_t* request;
    usb_request_complete_callback_t complete_cb;
  };

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_USB};
    config.callbacks[ZX_PROTOCOL_USB] = banjo_server_.callback();
    return config;
  }

  size_t bulk_out_request_count() {
    std::lock_guard lock(mutex_);
    return bulk_out_requests_.size();
  }

  std::vector<QueuedRequest> TakeBulkOutRequests() {
    std::lock_guard lock(mutex_);
    return std::exchange(bulk_out_requests_, {});
  }

  zx_status_t UsbControlIn(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                           int64_t timeout, uint8_t* out_read_buffer, size_t read_size,
                           size_t* out_read_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbControlOut(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                            int64_t timeout, const uint8_t* write_buffer, size_t write_size) {
    return ZX_OK;
  }

  void UsbRequestQueue(usb_request_t* request, const usb_request_complete_callback_t* completion) {
    std::lock_guard lock(mutex_);
    if (request->header.ep_address == kBulkOutAddr) {
      bulk_out_requests_.push_back({request, *completion});
    } else {
      bulk_in_requests_.push_back({request, *completion});
    }
  }

  usb_speed_t UsbGetSpeed() { return USB_SPEED_HIGH; }

  zx_status_t UsbSetInterface(uint8_t interface_number, uint8_t alt_setting) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  uint8_t UsbGetConfiguration() { return 0; }

  zx_status_t UsbSetConfiguration(uint8_t configuration) { return ZX_ERR_NOT_SUPPORTED; }

  zx_status_t UsbEnableEndpoint(const usb_endpoint_descriptor_t* ep_desc,
                                const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbResetEndpoint(uint8_t ep_address) { return ZX_ERR_NOT_SUPPORTED; }

  zx_status_t UsbResetDevice() { return ZX_ERR_NOT_SUPPORTED; }

  size_t UsbGetMaxTransferSize(uint8_t ep_address) { return 0; }

  uint32_t UsbGetDeviceId() { return 0; }

  void UsbGetDeviceDescriptor(usb_device_descriptor_t* out_desc) {}

  zx_status_t UsbGetConfigurationDescriptorLength(uint8_t configuration, size_t* out_length) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbGetConfigurationDescriptor(uint8_t configuration, uint8_t* out_desc_buffer,
                                            size_t desc_size, size_t* out_desc_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  size_t UsbGetDescriptorsLength() { return sizeof(descriptors_); }

  void UsbGetDescriptors(uint8_t* out_descs_buffer, size_t descs_size, size_t* out_descs_actual) {
    *out_descs_actual = std::min(descs_size, sizeof(descriptors_));
    memcpy(out_descs_buffer, &descriptors_, *out_descs_actual);
  }

  zx_status_t UsbGetStringDescriptor(uint8_t desc_id, uint16_t lang_id, uint16_t* out_lang_id,
                                     uint8_t* out_string_buffer, size_t string_size,
                                     size_t* out_string_actual) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbCancelAll(uint8_t ep_address) {
    std::vector<QueuedRequest> canceled;
    {
      std::lock_guard lock(mutex_);
      canceled =
          std::exchange(ep_address == kBulkOutAddr ? bulk_out_requests_ : bulk_in_requests_, {});
    }
    for (QueuedRequest& queued : canceled) {
      usb_request_complete(queued.request, ZX_ERR_IO_NOT_PRESENT, 0, &queued.complete_cb);
    }
    return ZX_OK;
  }

  uint64_t UsbGetCurrentFrame() { return 0; }

  size_t UsbGetRequestSize() { return sizeof(usb_request_t); }

 private:
  compat::BanjoServer banjo_server_{ZX_PROTOCOL_USB, this, &usb_protocol_ops_};

  [[maybe_unused]] struct {
    usb_interface_descriptor_t comm_interface;
    usb_interface_descriptor_t data_interface;
    usb_endpoint_descriptor_t bulk_in;
    usb_endpoint_descriptor_t bulk_out;
  } __PACKED descriptors_ = {
      .comm_interface =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 0,
              .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
              .b_interface_sub_class = fidl::ToUnderlying(fdescriptor::CdcSubclass::kAbstract),
              .b_interface_protocol = 1,
              .i_interface = 0,
          },
      .data_interface =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 1,
              .b_alternate_setting = 0,
              .b_num_endpoints = 2,
              .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
              .b_interface_sub_class = 0,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
      .bulk_in =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = kBulkInAddr,
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(512),
              .b_interval = 0,
          },
      .bulk_out =
          {
              .b_length = sizeof(usb_endpoint_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
              .b_endpoint_address = kBulkOutAddr,
              .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
              .w_max_packet_size = htole16(512),
              .b_interval = 0,
          },
  };

  std::mutex mutex_;
  std::vector<QueuedRequest> bulk_in_requests_ __TA_GUARDED(mutex_);
  std::vector<QueuedRequest> bulk_out_requests_ __TA_GUARDED(mutex_);
};

struct UsbCdcAcmTestEnvironment : public fdf_testing::Environment {
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    device_server_.Initialize(component::kDefaultInstance, std::nullopt,
                              fake_usb_.GetBanjoConfig());
    return zx::make_result(
        device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs));
  }

  FakeUsb fake_usb_;
  compat::DeviceServer device_server_;
};

class FixtureConfig final {
 public:
  using DriverType = UsbCdcAcmDevice;
  using EnvironmentType = UsbCdcAcmTestEnvironment;
};

class UsbCdcAcmDeviceTest : public ::testing::Test {
 public:
  void SetUp() override {
    ASSERT_OK(driver_test_.StartDriver().status_value());
    zx::result client = driver_test_.Connect<fuchsia_hardware_serialimpl::Service::Device>(
        UsbCdcAcmDevice::kDriverName);
    ASSERT_OK(client.status_value());
    client_.Bind(*std::move(client));
  }

  void TearDown() override { ASSERT_OK(driver_test_.StopDriver().status_value()); }

 protected:
  template <typename Callable>
  auto fake_usb(Callable&& callable) {
    return driver_test_.RunInEnvironmentTypeContext<std::invoke_result_t<Callable, FakeUsb&>>(
        [callback = std::forward<Callable>(callable)](UsbCdcAcmTestEnvironment& env) mutable {
          return callback(env.fake_usb_);
        });
  }

  fdf_testing::BackgroundDriverTest<FixtureConfig> driver_test_;
  fdf::WireSyncClient<fuchsia_hardware_serialimpl::Device> client_;
};

TEST_F(UsbCdcAcmDeviceTest, Lifetime) {}

TEST_F(UsbCdcAcmDeviceTest, RequestLengthIsBytesCopied) {
  std::vector<uint8_t> write_data(kWriteSize);
  for (size_t i = 0; i < write_data.size(); i++) {
    write_data[i] = static_cast<uint8_t>(i);
  }

  zx_status_t write_status = ZX_ERR_INTERNAL;
  std::thread writer([&]() {
    fdf::Arena arena('TEST');
    auto result = client_.buffer(arena)->Write(fidl::VectorView<uint8_t>::FromExternal(write_data));
    if (!result.ok()) {
      write_status = result.status();
    } else if (result->is_error()) {
      write_status = result->error_value();
    } else {
      write_status = ZX_OK;
    }
  });

  driver_test_.runtime().RunUntil(
      [&]() {
        return fake_usb([](FakeUsb& usb) { return usb.bulk_out_request_count(); }) ==
               kExpectedWriteRequests;
      },
      kEnvironmentPollingPeriod);

  std::vector<FakeUsb::QueuedRequest> requests =
      fake_usb([](FakeUsb& usb) { return usb.TakeBulkOutRequests(); });
  EXPECT_EQ(requests.size(), kExpectedWriteRequests);

  std::vector<uint8_t> sent_data;
  for (const FakeUsb::QueuedRequest& queued : requests) {
    usb_request_t* request = queued.request;
    const size_t expected_length =
        std::min<size_t>(write_data.size() - sent_data.size(), request->size);
    EXPECT_LE(request->header.length, request->size);
    EXPECT_EQ(request->header.length, expected_length);

    const size_t offset = sent_data.size();
    sent_data.resize(offset + expected_length);
    EXPECT_EQ(usb_request_copy_from(request, sent_data.data() + offset, expected_length, 0),
              static_cast<ssize_t>(expected_length));
  }
  EXPECT_EQ(sent_data, write_data);

  for (FakeUsb::QueuedRequest& queued : requests) {
    driver_test_.RunInDriverContext([&](UsbCdcAcmDevice&) {
      usb_request_complete(queued.request, ZX_OK, queued.request->header.length,
                           &queued.complete_cb);
    });
  }

  writer.join();
  EXPECT_OK(write_status);
}

TEST_F(UsbCdcAcmDeviceTest, WriteCompleteRefillsRemainingData) {
  constexpr size_t kLargeWriteSize = (kWriteRequestCount + 1) * kUsbBufferSize + 808;
  std::vector<uint8_t> write_data(kLargeWriteSize);
  for (size_t i = 0; i < write_data.size(); i++) {
    write_data[i] = static_cast<uint8_t>((i * 31) + 7);
  }

  zx_status_t write_status = ZX_ERR_INTERNAL;
  std::thread writer([&]() {
    fdf::Arena arena('TEST');
    auto result = client_.buffer(arena)->Write(fidl::VectorView<uint8_t>::FromExternal(write_data));
    if (!result.ok()) {
      write_status = result.status();
    } else if (result->is_error()) {
      write_status = result->error_value();
    } else {
      write_status = ZX_OK;
    }
  });

  driver_test_.runtime().RunUntil(
      [&]() {
        return fake_usb([](FakeUsb& usb) { return usb.bulk_out_request_count(); }) ==
               kWriteRequestCount;
      },
      kEnvironmentPollingPeriod);

  std::vector<uint8_t> sent_data;
  while (fake_usb([](FakeUsb& usb) { return usb.bulk_out_request_count(); }) > 0) {
    std::vector<FakeUsb::QueuedRequest> requests =
        fake_usb([](FakeUsb& usb) { return usb.TakeBulkOutRequests(); });
    for (FakeUsb::QueuedRequest& queued : requests) {
      usb_request_t* request = queued.request;
      const size_t expected_length =
          std::min<size_t>(write_data.size() - sent_data.size(), request->size);
      EXPECT_LE(request->header.length, request->size);
      EXPECT_EQ(request->header.length, expected_length);

      const size_t offset = sent_data.size();
      sent_data.resize(offset + expected_length);
      EXPECT_EQ(usb_request_copy_from(request, sent_data.data() + offset, expected_length, 0),
                static_cast<ssize_t>(expected_length));

      // Note that the completion callback may re-fill the request queue, which is why draining the
      // queue is done in a loop.
      driver_test_.RunInDriverContext([&](UsbCdcAcmDevice&) {
        usb_request_complete(request, ZX_OK, request->header.length, &queued.complete_cb);
      });
    }
  }

  writer.join();
  EXPECT_OK(write_status);
  EXPECT_EQ(sent_data, write_data);
}

}  // namespace
}  // namespace usb_cdc_acm_serial
