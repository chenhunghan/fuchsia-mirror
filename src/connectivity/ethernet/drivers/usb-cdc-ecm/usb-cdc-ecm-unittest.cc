// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/ethernet/drivers/usb-cdc-ecm/usb-cdc-ecm.h"

#include <lib/async/cpp/task.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fit/defer.h>
#include <lib/sync/completion.h>

#include <atomic>
#include <deque>
#include <functional>
#include <thread>
#include <vector>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"
#include "usb-cdc-ecm-lib.h"
#include "usb/usb.h"

namespace {
namespace fdescriptor = fuchsia_hardware_usb_descriptor;

class UsbCdcEcmTest : public ::testing::Test {
 public:
  void SetUp() override {
    proto_.ops = &ops_;
    proto_.ctx = this;
    ops_.get_descriptors_length = UsbGetDescriptorsLength;
    ops_.get_descriptors = UsbGetDescriptors;
    ops_.control_in = UsbControlIn;
    ops_.control_out = UsbControlOut;
    iter = {};
  }

  void TearDown() override {
    if (iter.desc) {
      usb_desc_iter_release(&iter);
    }
  }

 protected:
  static void UsbGetDescriptors(void* ctx, uint8_t* out_descs_buffer, size_t descs_size,
                                size_t* out_descs_actual) {
    auto test = reinterpret_cast<UsbCdcEcmTest*>(ctx);
    size_t descriptors_length = test->GetDescriptorLength();
    if (descs_size < descriptors_length) {
      descriptors_length = descs_size;
    }
    memcpy(out_descs_buffer, test->GetDescriptors(), descriptors_length);
    *out_descs_actual = descriptors_length;
  }

  static size_t UsbGetDescriptorsLength(void* ctx) {
    auto test = reinterpret_cast<UsbCdcEcmTest*>(ctx);
    return test->GetDescriptorLength();
  }

  static zx_status_t UsbControlIn(void* ctx, uint8_t request_type, uint8_t request, uint16_t value,
                                  uint16_t index, int64_t timeout, uint8_t* out_read_buffer,
                                  size_t read_size, size_t* out_read_actual) {
    if ((request_type & fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn)) == 0 ||
        request != fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor)) {
      return ZX_ERR_INTERNAL;
    }
    const size_t expected_str_size = sizeof(usb_string_descriptor_t) + ETH_MAC_SIZE * 4;
    if (read_size < expected_str_size) {
      return ZX_ERR_INVALID_ARGS;
    }
    *out_read_actual = expected_str_size;
    usb_string_descriptor_t usb_str;
    usb_str.b_length = expected_str_size;
    usb_str.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kString);
    memcpy(out_read_buffer, &usb_str, sizeof(usb_string_descriptor_t));
    uint8_t* ptr = reinterpret_cast<uint8_t*>(out_read_buffer) + sizeof(usb_string_descriptor_t);
    for (size_t i = 0; i < ETH_MAC_SIZE; i++) {
      memcpy(ptr, "F\0F\0", 4);
      ptr += 4;
    }

    return ZX_OK;
  }

  void SetDescriptors(void* descriptors) { descriptors_ = descriptors; }

  void* GetDescriptors() { return descriptors_; }

  void SetDescriptorLength(size_t descriptor_length) { descriptor_length_ = descriptor_length; }

  static zx_status_t UsbControlOut(void* ctx, uint8_t request_type, uint8_t request, uint16_t value,
                                   uint16_t index, int64_t timeout, const uint8_t* write_buffer,
                                   size_t write_size) {
    auto test = reinterpret_cast<UsbCdcEcmTest*>(ctx);
    test->last_control_out_index_ = index;
    test->last_control_out_request_ = request;
    return ZX_OK;
  }

  size_t GetDescriptorLength() { return descriptor_length_; }

  usb_protocol_t* GetUsbProto() { return &proto_; }

  uint16_t last_control_out_index_ = 0;
  uint8_t last_control_out_request_ = 0;
  usb_protocol_t proto_{};
  usb_protocol_ops_t ops_{};
  void* descriptors_ = nullptr;
  size_t descriptor_length_ = 0;
  usb_desc_iter_t iter;

  // The code under test logs through the DFv2 logger, which requires a global logger instance to
  // be set. These tests do not run inside a driver host, so provide one here.
  // Suppress ERROR logs so error-path test cases don't fail Fuchsia test runner log severity
  // checks.
  fdf_testing::ScopedGlobalLogger logger_{FUCHSIA_LOG_FATAL};
};

TEST_F(UsbCdcEcmTest, ParseUsbDescriptorTest) {
  std::vector<uint8_t> buffer;
  usb_interface_descriptor_t test_default_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_default_ifc),
                reinterpret_cast<uint8_t*>(&test_default_ifc) + sizeof(test_default_ifc));
  usb_interface_descriptor_t test_interrupt_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 1,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_interrupt_ifc),
                reinterpret_cast<uint8_t*>(&test_interrupt_ifc) + sizeof(test_interrupt_ifc));
  usb_cs_header_interface_descriptor_t test_cdc_header_desc = {
      .bLength = sizeof(usb_cs_header_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kHeader),
      .bcdCDC = 0x0110,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_cdc_header_desc),
                reinterpret_cast<uint8_t*>(&test_cdc_header_desc) + sizeof(test_cdc_header_desc));
  usb_cs_ethernet_interface_descriptor_t test_cdc_eth_ifc = {
      .bLength = sizeof(usb_cs_ethernet_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kEthernet),
      .iMACAddress = 1,
      .bmEthernetStatistics = 0,
      .wMaxSegmentSize = 1,
      .wNumberMCFilters = 0,
      .bNumberPowerFilters = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_cdc_eth_ifc),
                reinterpret_cast<uint8_t*>(&test_cdc_eth_ifc) + sizeof(test_cdc_eth_ifc));
  usb_endpoint_descriptor_t test_int_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_int_ep),
                reinterpret_cast<uint8_t*>(&test_int_ep) + sizeof(test_int_ep));
  usb_interface_descriptor_t test_data_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 2,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_data_ifc),
                reinterpret_cast<uint8_t*>(&test_data_ifc) + sizeof(test_data_ifc));
  usb_endpoint_descriptor_t test_in_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_in_ep),
                reinterpret_cast<uint8_t*>(&test_in_ep) + sizeof(test_in_ep));
  usb_endpoint_descriptor_t test_out_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_out_ep),
                reinterpret_cast<uint8_t*>(&test_out_ep) + sizeof(test_out_ep));

  SetDescriptors(buffer.data());
  SetDescriptorLength(buffer.size());
  usb::UsbDevice usb = usb::UsbDevice(GetUsbProto());
  auto parser = usb_cdc_ecm::UsbCdcDescriptorParser::Parse(usb);

  ASSERT_OK(parser.status_value());
}

TEST_F(UsbCdcEcmTest, ParseUsbDescriptorZeroMacIndexTest) {
  std::vector<uint8_t> buffer;
  usb_interface_descriptor_t test_default_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_default_ifc),
                reinterpret_cast<uint8_t*>(&test_default_ifc) + sizeof(test_default_ifc));
  usb_interface_descriptor_t test_interrupt_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 1,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_interrupt_ifc),
                reinterpret_cast<uint8_t*>(&test_interrupt_ifc) + sizeof(test_interrupt_ifc));
  usb_cs_header_interface_descriptor_t test_cdc_header_desc = {
      .bLength = sizeof(usb_cs_header_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kHeader),
      .bcdCDC = 0x0110,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_cdc_header_desc),
                reinterpret_cast<uint8_t*>(&test_cdc_header_desc) + sizeof(test_cdc_header_desc));
  usb_cs_ethernet_interface_descriptor_t test_cdc_eth_ifc = {
      .bLength = sizeof(usb_cs_ethernet_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kEthernet),
      .iMACAddress = 0,
      .bmEthernetStatistics = 0,
      .wMaxSegmentSize = 1,
      .wNumberMCFilters = 0,
      .bNumberPowerFilters = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_cdc_eth_ifc),
                reinterpret_cast<uint8_t*>(&test_cdc_eth_ifc) + sizeof(test_cdc_eth_ifc));
  usb_endpoint_descriptor_t test_int_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_int_ep),
                reinterpret_cast<uint8_t*>(&test_int_ep) + sizeof(test_int_ep));
  usb_interface_descriptor_t test_data_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 2,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_data_ifc),
                reinterpret_cast<uint8_t*>(&test_data_ifc) + sizeof(test_data_ifc));
  usb_endpoint_descriptor_t test_in_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_in_ep),
                reinterpret_cast<uint8_t*>(&test_in_ep) + sizeof(test_in_ep));
  usb_endpoint_descriptor_t test_out_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_out_ep),
                reinterpret_cast<uint8_t*>(&test_out_ep) + sizeof(test_out_ep));

  SetDescriptors(buffer.data());
  SetDescriptorLength(buffer.size());
  usb::UsbDevice usb = usb::UsbDevice(GetUsbProto());
  auto parser = usb_cdc_ecm::UsbCdcDescriptorParser::Parse(usb);

  ASSERT_EQ(ZX_ERR_NOT_SUPPORTED, parser.status_value());
}

TEST_F(UsbCdcEcmTest, CommInterfaceIndexGreaterThanZeroTest) {
  std::vector<uint8_t> buffer;
  usb_interface_descriptor_t test_default_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_default_ifc),
                reinterpret_cast<uint8_t*>(&test_default_ifc) + sizeof(test_default_ifc));
  usb_interface_descriptor_t test_interrupt_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 2,
      .b_alternate_setting = 0,
      .b_num_endpoints = 1,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_interrupt_ifc),
                reinterpret_cast<uint8_t*>(&test_interrupt_ifc) + sizeof(test_interrupt_ifc));
  usb_cs_header_interface_descriptor_t test_cdc_header_desc = {
      .bLength = sizeof(usb_cs_header_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kHeader),
      .bcdCDC = 0x0110,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_cdc_header_desc),
                reinterpret_cast<uint8_t*>(&test_cdc_header_desc) + sizeof(test_cdc_header_desc));
  usb_cs_ethernet_interface_descriptor_t test_cdc_eth_ifc = {
      .bLength = sizeof(usb_cs_ethernet_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kEthernet),
      .iMACAddress = 1,
      .bmEthernetStatistics = 0,
      .wMaxSegmentSize = 1,
      .wNumberMCFilters = 0,
      .bNumberPowerFilters = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_cdc_eth_ifc),
                reinterpret_cast<uint8_t*>(&test_cdc_eth_ifc) + sizeof(test_cdc_eth_ifc));
  usb_endpoint_descriptor_t test_int_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_int_ep),
                reinterpret_cast<uint8_t*>(&test_int_ep) + sizeof(test_int_ep));
  usb_interface_descriptor_t test_data_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 3,
      .b_alternate_setting = 0,
      .b_num_endpoints = 2,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_data_ifc),
                reinterpret_cast<uint8_t*>(&test_data_ifc) + sizeof(test_data_ifc));
  usb_endpoint_descriptor_t test_in_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_in_ep),
                reinterpret_cast<uint8_t*>(&test_in_ep) + sizeof(test_in_ep));
  usb_endpoint_descriptor_t test_out_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut),
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 0,
      .b_interval = 0,
  };
  buffer.insert(buffer.end(), reinterpret_cast<uint8_t*>(&test_out_ep),
                reinterpret_cast<uint8_t*>(&test_out_ep) + sizeof(test_out_ep));

  SetDescriptors(buffer.data());
  SetDescriptorLength(buffer.size());
  usb::UsbDevice usb = usb::UsbDevice(GetUsbProto());
  auto parser = usb_cdc_ecm::UsbCdcDescriptorParser::Parse(usb);

  ASSERT_OK(parser.status_value());
  EXPECT_EQ(parser->GetCommInterface().number, 2);

  zx_status_t status = usb.ControlOut(kClassInterfaceOut, 0, 0, parser->GetCommInterface().number,
                                      ZX_TIME_INFINITE, nullptr, 0);
  EXPECT_OK(status);
  EXPECT_EQ(last_control_out_index_, 2);
}

constexpr uint8_t kTestIntEpAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn) | 0x01;
constexpr uint8_t kTestRxEpAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kIn) | 0x02;
constexpr uint8_t kTestTxEpAddr = fidl::ToUnderlying(fdescriptor::EndpointDirection::kOut) | 0x03;
constexpr uint16_t kTestMtu = 32768;

std::vector<uint8_t> BuildCommDescriptors() {
  std::vector<uint8_t> buffer;
  auto append = [&buffer](const auto& desc) {
    const auto* bytes = reinterpret_cast<const uint8_t*>(&desc);
    buffer.insert(buffer.end(), bytes, bytes + sizeof(desc));
  };

  usb_interface_descriptor_t interrupt_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 0,
      .b_alternate_setting = 0,
      .b_num_endpoints = 1,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
      .b_interface_sub_class = fidl::ToUnderlying(fdescriptor::CdcSubclass::kEthernet),
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  append(interrupt_ifc);

  usb_cs_header_interface_descriptor_t cdc_header_desc = {
      .bLength = sizeof(usb_cs_header_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kHeader),
      .bcdCDC = 0x0110,
  };
  append(cdc_header_desc);

  usb_cs_ethernet_interface_descriptor_t cdc_eth_ifc = {
      .bLength = sizeof(usb_cs_ethernet_interface_descriptor_t),
      .bDescriptorType = fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface),
      .bDescriptorSubType = fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kEthernet),
      .iMACAddress = 1,
      .bmEthernetStatistics = 0,
      .wMaxSegmentSize = kTestMtu,
      .wNumberMCFilters = 0,
      .bNumberPowerFilters = 0,
  };
  append(cdc_eth_ifc);

  usb_endpoint_descriptor_t int_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = kTestIntEpAddr,
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt),
      .w_max_packet_size = 64,
      .b_interval = 8,
  };
  append(int_ep);

  return buffer;
}

std::vector<uint8_t> BuildDataDescriptors() {
  std::vector<uint8_t> buffer;
  auto append = [&buffer](const auto& desc) {
    const auto* bytes = reinterpret_cast<const uint8_t*>(&desc);
    buffer.insert(buffer.end(), bytes, bytes + sizeof(desc));
  };

  usb_interface_descriptor_t default_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 1,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  append(default_ifc);

  usb_interface_descriptor_t data_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 1,
      .b_alternate_setting = 1,
      .b_num_endpoints = 2,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  append(data_ifc);

  usb_endpoint_descriptor_t in_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = kTestRxEpAddr,
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 512,
      .b_interval = 0,
  };
  append(in_ep);

  usb_endpoint_descriptor_t out_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = kTestTxEpAddr,
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 512,
      .b_interval = 0,
  };
  append(out_ep);

  return buffer;
}

std::vector<uint8_t> BuildValidCdcEcmDescriptors() {
  std::vector<uint8_t> buffer = BuildCommDescriptors();
  std::vector<uint8_t> data = BuildDataDescriptors();
  buffer.insert(buffer.end(), data.begin(), data.end());
  return buffer;
}

struct QueuedUsbRequest {
  usb_request_t* req;
  usb_request_complete_callback_t cb;
};

class FakeUsbParentServer {
 public:
  FakeUsbParentServer() : descriptors_(BuildValidCdcEcmDescriptors()) {
    usb_ops_.get_descriptors_length = UsbGetDescriptorsLength;
    usb_ops_.get_descriptors = UsbGetDescriptors;
    usb_ops_.control_in = UsbControlIn;
    usb_ops_.control_out = UsbControlOut;
    usb_ops_.set_interface = UsbSetInterface;
    usb_ops_.get_request_size = UsbGetRequestSize;
    usb_ops_.request_queue = UsbRequestQueue;
    usb_ops_.cancel_all = UsbCancelAll;
    usb_ops_.reset_endpoint = UsbResetEndpoint;

    composite_ops_.get_additional_descriptor_length = CompositeGetAdditionalDescriptorLength;
    composite_ops_.get_additional_descriptor_list = CompositeGetAdditionalDescriptorList;
    composite_ops_.claim_interface = CompositeClaimInterface;
  }

  compat::DeviceServer::BanjoConfig GetBanjoConfig() {
    compat::DeviceServer::BanjoConfig config{ZX_PROTOCOL_USB};
    config.callbacks[ZX_PROTOCOL_USB] = usb_banjo_server_.callback();
    config.callbacks[ZX_PROTOCOL_USB_COMPOSITE] = composite_banjo_server_.callback();
    return config;
  }

  size_t QueuedCount(uint8_t ep_addr) const {
    fbl::AutoLock lock(&mutex_);
    return QueueForLocked(ep_addr).size();
  }

  size_t CancelAllCount(uint8_t ep_addr) const {
    fbl::AutoLock lock(&mutex_);
    if (ep_addr == kTestIntEpAddr) {
      return int_cancel_count_;
    }
    if (ep_addr == kTestRxEpAddr) {
      return rx_cancel_count_;
    }
    if (ep_addr == kTestTxEpAddr) {
      return tx_cancel_count_;
    }
    return 0;
  }

  size_t ResetEndpointCount(uint8_t ep_addr) const {
    fbl::AutoLock lock(&mutex_);
    if (ep_addr == kTestIntEpAddr) {
      return int_reset_count_;
    }
    if (ep_addr == kTestRxEpAddr) {
      return rx_reset_count_;
    }
    if (ep_addr == kTestTxEpAddr) {
      return tx_reset_count_;
    }
    return 0;
  }

  uint16_t last_packet_filter() const {
    fbl::AutoLock lock(&mutex_);
    return last_packet_filter_;
  }

  void set_control_out_status(zx_status_t status) {
    fbl::AutoLock lock(&mutex_);
    control_out_status_ = status;
  }

  void set_inline_completion_status(std::optional<zx_status_t> status) {
    fbl::AutoLock lock(&mutex_);
    inline_completion_status_ = status;
  }

  void set_on_cancel_all(std::function<void(uint8_t)> cb) {
    fbl::AutoLock lock(&mutex_);
    on_cancel_all_ = std::move(cb);
  }

  void set_descriptors(std::vector<uint8_t> descriptors) {
    fbl::AutoLock lock(&mutex_);
    descriptors_ = std::move(descriptors);
  }

  void set_additional_descriptors(std::vector<uint8_t> additional_descriptors) {
    fbl::AutoLock lock(&mutex_);
    additional_descriptors_ = std::move(additional_descriptors);
  }

  size_t claimed_interface_count() const {
    fbl::AutoLock lock(&mutex_);
    return claimed_interface_count_;
  }

  std::optional<QueuedUsbRequest> PopRequest(uint8_t ep_addr) {
    fbl::AutoLock lock(&mutex_);
    auto& q = QueueForLocked(ep_addr);
    if (q.empty()) {
      return std::nullopt;
    }
    QueuedUsbRequest item = q.front();
    q.pop_front();
    return item;
  }

 private:
  const std::deque<QueuedUsbRequest>& QueueForLocked(uint8_t ep_addr) const __TA_REQUIRES(mutex_) {
    if (ep_addr == kTestIntEpAddr) {
      return int_requests_;
    }
    if (ep_addr == kTestRxEpAddr) {
      return rx_requests_;
    }
    return tx_requests_;
  }

  std::deque<QueuedUsbRequest>& QueueForLocked(uint8_t ep_addr) __TA_REQUIRES(mutex_) {
    if (ep_addr == kTestIntEpAddr) {
      return int_requests_;
    }
    if (ep_addr == kTestRxEpAddr) {
      return rx_requests_;
    }
    return tx_requests_;
  }

  static size_t UsbGetDescriptorsLength(void* ctx) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    return self->descriptors_.size();
  }

  static void UsbGetDescriptors(void* ctx, uint8_t* out_descs_buffer, size_t descs_size,
                                size_t* out_descs_actual) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    size_t len = std::min(descs_size, self->descriptors_.size());
    memcpy(out_descs_buffer, self->descriptors_.data(), len);
    *out_descs_actual = len;
  }

  static zx_status_t UsbControlIn(void* ctx, uint8_t request_type, uint8_t request, uint16_t value,
                                  uint16_t index, int64_t timeout, uint8_t* out_read_buffer,
                                  size_t read_size, size_t* out_read_actual) {
    const size_t expected_str_size = sizeof(usb_string_descriptor_t) + ETH_MAC_SIZE * 4;
    if (read_size < expected_str_size) {
      return ZX_ERR_INVALID_ARGS;
    }
    *out_read_actual = expected_str_size;
    usb_string_descriptor_t usb_str = {
        .b_length = static_cast<uint8_t>(expected_str_size),
        .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kString),
    };
    memcpy(out_read_buffer, &usb_str, sizeof(usb_string_descriptor_t));
    uint8_t* ptr = out_read_buffer + sizeof(usb_string_descriptor_t);
    for (size_t i = 0; i < ETH_MAC_SIZE; i++) {
      memcpy(ptr, "F\0F\0", 4);
      ptr += 4;
    }
    return ZX_OK;
  }

  static zx_status_t UsbControlOut(void* ctx, uint8_t request_type, uint8_t request, uint16_t value,
                                   uint16_t index, int64_t timeout, const uint8_t* write_buffer,
                                   size_t write_size) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    if (self->control_out_status_ != ZX_OK) {
      return self->control_out_status_;
    }
    if (request == fidl::ToUnderlying(fdescriptor::CdcRequest::kSetEthernetPacketFilter)) {
      self->last_packet_filter_ = value;
    }
    return ZX_OK;
  }

  static zx_status_t UsbSetInterface(void* ctx, uint8_t interface_number, uint8_t alt_setting) {
    return ZX_OK;
  }

  static size_t UsbGetRequestSize(void* ctx) { return sizeof(usb_request_t); }

  static void UsbRequestQueue(void* ctx, usb_request_t* usb_request,
                              const usb_request_complete_callback_t* complete_cb) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    std::optional<zx_status_t> inline_status;
    {
      fbl::AutoLock lock(&self->mutex_);
      inline_status = self->inline_completion_status_;
      if (!inline_status.has_value()) {
        auto& queue = self->QueueForLocked(usb_request->header.ep_address);
        queue.push_back({usb_request, *complete_cb});
        return;
      }
    }
    usb_request_complete(usb_request, *inline_status, 0, complete_cb);
  }

  static zx_status_t UsbCancelAll(void* ctx, uint8_t ep_address) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    std::deque<QueuedUsbRequest> to_cancel;
    std::function<void(uint8_t)> on_cancel_all;
    {
      fbl::AutoLock lock(&self->mutex_);
      if (ep_address == kTestIntEpAddr) {
        self->int_cancel_count_++;
      } else if (ep_address == kTestRxEpAddr) {
        self->rx_cancel_count_++;
      } else if (ep_address == kTestTxEpAddr) {
        self->tx_cancel_count_++;
      }
      to_cancel = std::move(self->QueueForLocked(ep_address));
      on_cancel_all = self->on_cancel_all_;
    }
    for (auto& item : to_cancel) {
      usb_request_complete(item.req, ZX_ERR_CANCELED, 0, &item.cb);
    }
    if (on_cancel_all) {
      on_cancel_all(ep_address);
    }
    return ZX_OK;
  }

  static zx_status_t UsbResetEndpoint(void* ctx, uint8_t ep_address) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    if (ep_address == kTestIntEpAddr) {
      self->int_reset_count_++;
    } else if (ep_address == kTestRxEpAddr) {
      self->rx_reset_count_++;
    } else if (ep_address == kTestTxEpAddr) {
      self->tx_reset_count_++;
    }
    return ZX_OK;
  }

  static size_t CompositeGetAdditionalDescriptorLength(void* ctx) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    return self->additional_descriptors_.size();
  }

  static zx_status_t CompositeGetAdditionalDescriptorList(void* ctx, uint8_t* out_descs_buffer,
                                                          size_t descs_size,
                                                          size_t* out_descs_actual) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    if (descs_size < self->additional_descriptors_.size()) {
      return ZX_ERR_BUFFER_TOO_SMALL;
    }
    size_t len = self->additional_descriptors_.size();
    if (len > 0) {
      memcpy(out_descs_buffer, self->additional_descriptors_.data(), len);
    }
    *out_descs_actual = len;
    return ZX_OK;
  }

  static zx_status_t CompositeClaimInterface(void* ctx, const usb_interface_descriptor_t* desc,
                                             uint32_t length) {
    auto* self = static_cast<FakeUsbParentServer*>(ctx);
    fbl::AutoLock lock(&self->mutex_);
    self->claimed_interface_count_++;
    const auto* bytes = reinterpret_cast<const uint8_t*>(desc);
    self->descriptors_.insert(self->descriptors_.end(), bytes, bytes + length);
    return ZX_OK;
  }

  mutable fbl::Mutex mutex_;
  std::vector<uint8_t> descriptors_ __TA_GUARDED(mutex_);
  std::vector<uint8_t> additional_descriptors_ __TA_GUARDED(mutex_);
  size_t claimed_interface_count_ __TA_GUARDED(mutex_) = 0;
  usb_protocol_ops_t usb_ops_{};
  usb_composite_protocol_ops_t composite_ops_{};
  compat::BanjoServer usb_banjo_server_{ZX_PROTOCOL_USB, this, &usb_ops_};
  compat::BanjoServer composite_banjo_server_{ZX_PROTOCOL_USB_COMPOSITE, this, &composite_ops_};
  std::deque<QueuedUsbRequest> int_requests_ __TA_GUARDED(mutex_);
  std::deque<QueuedUsbRequest> rx_requests_ __TA_GUARDED(mutex_);
  std::deque<QueuedUsbRequest> tx_requests_ __TA_GUARDED(mutex_);
  size_t int_cancel_count_ __TA_GUARDED(mutex_) = 0;
  size_t rx_cancel_count_ __TA_GUARDED(mutex_) = 0;
  size_t tx_cancel_count_ __TA_GUARDED(mutex_) = 0;
  size_t int_reset_count_ __TA_GUARDED(mutex_) = 0;
  size_t rx_reset_count_ __TA_GUARDED(mutex_) = 0;
  size_t tx_reset_count_ __TA_GUARDED(mutex_) = 0;
  uint16_t last_packet_filter_ __TA_GUARDED(mutex_) = 0;
  zx_status_t control_out_status_ __TA_GUARDED(mutex_) = ZX_OK;
  // When set, UsbRequestQueue() completes each request with this status before returning.
  std::optional<zx_status_t> inline_completion_status_ __TA_GUARDED(mutex_);
  std::function<void(uint8_t)> on_cancel_all_ __TA_GUARDED(mutex_);
};

class UsbCdcEcmTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    device_server_.Initialize("default", std::nullopt, usb_server_.GetBanjoConfig());
    ZX_ASSERT(device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   &to_driver_vfs) == ZX_OK);
    return zx::ok();
  }

  FakeUsbParentServer& usb_server() { return usb_server_; }

 private:
  compat::DeviceServer device_server_;
  FakeUsbParentServer usb_server_;
};

class UsbCdcEcmFixtureConfig final {
 public:
  using DriverType = usb_cdc_ecm::UsbCdcEcm;
  using EnvironmentType = UsbCdcEcmTestEnvironment;
};

class UsbCdcEcmDriverTest : public ::testing::Test {
 public:
  void SetUp() override {
    ASSERT_OK(driver_test_.StartDriver().status_value());
    driver_started_ = true;
  }

  void TearDown() override {
    if (driver_started_) {
      ASSERT_OK(driver_test_.StopDriver().status_value());
      driver_started_ = false;
    }
  }

  fdf_testing::ForegroundDriverTest<UsbCdcEcmFixtureConfig>& driver_test() { return driver_test_; }

  size_t QueuedCount(uint8_t ep_addr) {
    size_t count = 0;
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbCdcEcmTestEnvironment& env) { count = env.usb_server().QueuedCount(ep_addr); });
    return count;
  }

  size_t CancelAllCount(uint8_t ep_addr) {
    size_t count = 0;
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbCdcEcmTestEnvironment& env) { count = env.usb_server().CancelAllCount(ep_addr); });
    return count;
  }

  size_t ResetEndpointCount(uint8_t ep_addr) {
    size_t count = 0;
    driver_test_.RunInEnvironmentTypeContext([&](UsbCdcEcmTestEnvironment& env) {
      count = env.usb_server().ResetEndpointCount(ep_addr);
    });
    return count;
  }

  uint16_t LastPacketFilter() {
    uint16_t filter = 0;
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbCdcEcmTestEnvironment& env) { filter = env.usb_server().last_packet_filter(); });
    return filter;
  }

  void SetControlOutStatus(zx_status_t status) {
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbCdcEcmTestEnvironment& env) { env.usb_server().set_control_out_status(status); });
  }

  void SetInlineCompletionStatus(std::optional<zx_status_t> status) {
    driver_test_.RunInEnvironmentTypeContext([&](UsbCdcEcmTestEnvironment& env) {
      env.usb_server().set_inline_completion_status(status);
    });
  }

  void SetOnCancelAll(std::function<void(uint8_t)> cb) {
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbCdcEcmTestEnvironment& env) { env.usb_server().set_on_cancel_all(std::move(cb)); });
  }

  std::optional<QueuedUsbRequest> PopRequest(uint8_t ep_addr) {
    std::optional<QueuedUsbRequest> req;
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbCdcEcmTestEnvironment& env) { req = env.usb_server().PopRequest(ep_addr); });
    return req;
  }

 protected:
  bool driver_started_ = false;

 private:
  fdf_testing::ForegroundDriverTest<UsbCdcEcmFixtureConfig> driver_test_;
};

TEST_F(UsbCdcEcmDriverTest, InterruptHandlingAndStopCancellation) {
  // Start() must configure int_wait_ and queue the initial interrupt request.
  ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);

  // Deliver a CONNECTION_SPEED_CHANGE interrupt notification and verify both upstream and
  // downstream speeds are updated independently.
  {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());

    usb_cdc_speed_change_notification_t packet = {
        .notification =
            {
                .bmRequestType = kClassInterfaceIn,
                .bNotification =
                    fidl::ToUnderlying(fdescriptor::CdcNotification::kConnectionSpeedChange),
                .wValue = 0,
                .wIndex = 0,
                .wLength = 8,
            },
        .downlink_br = 987'654'321u,
        .uplink_br = 123'456'789u,
    };
    ASSERT_EQ(usb_request_copy_to(int_req->req, &packet, sizeof(packet), 0),
              static_cast<ssize_t>(sizeof(packet)));
    usb_request_complete(int_req->req, ZX_OK, sizeof(packet), &int_req->cb);
  }
  driver_test().runtime().RunUntilIdle();

  EXPECT_EQ(driver_test().driver()->us_bps(), 123'456'789u);
  EXPECT_EQ(driver_test().driver()->ds_bps(), 987'654'321u);
  ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);

  // Deliver a NETWORK_CONNECTION (online) notification to queue RX requests.
  {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());

    usb_cdc_notification_t notif = {
        .bmRequestType = kClassInterfaceIn,
        .bNotification = fidl::ToUnderlying(fdescriptor::CdcNotification::kNetworkConnection),
        .wValue = 1,
        .wIndex = 0,
        .wLength = 0,
    };
    ASSERT_EQ(usb_request_copy_to(int_req->req, &notif, sizeof(notif), 0),
              static_cast<ssize_t>(sizeof(notif)));
    usb_request_complete(int_req->req, ZX_OK, sizeof(notif), &int_req->cb);
  }
  driver_test().runtime().RunUntilIdle();

  const size_t initial_rx_queued = QueuedCount(kTestRxEpAddr);
  ASSERT_GT(initial_rx_queued, 0u);
  ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);

  // Completing an RX request with ZX_ERR_CANCELED must return it to the pool without requeueing.
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    usb_request_complete(rx_req->req, ZX_ERR_CANCELED, 0, &rx_req->cb);
  }
  EXPECT_EQ(QueuedCount(kTestRxEpAddr), initial_rx_queued - 1);

  // Completing an RX request while the parent completes requeues inline with
  // ZX_ERR_IO_NOT_PRESENT must return the requeued request to the pool without deadlocking.
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    SetInlineCompletionStatus(ZX_ERR_IO_NOT_PRESENT);
    usb_request_complete(rx_req->req, ZX_OK, 0, &rx_req->cb);
    SetInlineCompletionStatus(std::nullopt);
  }
  EXPECT_EQ(QueuedCount(kTestRxEpAddr), initial_rx_queued - 2);

  // StopDriver() must cancel all three endpoints; canceled interrupt and RX completions must
  // terminate cleanly without requeueing.
  ASSERT_OK(driver_test().StopDriver().status_value());
  driver_started_ = false;
  driver_test().runtime().RunUntilIdle();

  EXPECT_EQ(CancelAllCount(kTestIntEpAddr), 1u);
  EXPECT_EQ(CancelAllCount(kTestTxEpAddr), 1u);
  EXPECT_EQ(CancelAllCount(kTestRxEpAddr), 1u);
  EXPECT_EQ(QueuedCount(kTestIntEpAddr), 0u);
  EXPECT_EQ(QueuedCount(kTestRxEpAddr), 0u);
}

TEST_F(UsbCdcEcmDriverTest, QueueTxAndStopDrainsPendingTxQueue) {
  usb_cdc_ecm::UsbCdcEcm* drv = driver_test().driver();

  ethernet_info_t info = {};
  ASSERT_OK(drv->EthernetImplQuery(0, &info));
  ASSERT_GE(info.netbuf_size, sizeof(ethernet_netbuf_t));

  uint8_t payload[64] = {1, 2, 3, 4};
  auto make_netbuf = [&](std::vector<uint8_t>& storage,
                         size_t len = sizeof(payload)) -> ethernet_netbuf_t* {
    storage.assign(info.netbuf_size, 0);
    auto* netbuf = reinterpret_cast<ethernet_netbuf_t*>(storage.data());
    netbuf->data_buffer = payload;
    netbuf->data_size = len;
    return netbuf;
  };
  auto tx_cb = [](void* cookie, zx_status_t status, ethernet_netbuf_t* netbuf) {
    *static_cast<std::optional<zx_status_t>*>(cookie) = status;
  };

  // QueueTx before EthernetImplStart must fail with ZX_ERR_BAD_STATE.
  {
    std::vector<uint8_t> buf;
    std::optional<zx_status_t> tx_status;
    drv->EthernetImplQueueTx(0, make_netbuf(buf), tx_cb, &tx_status);
    ASSERT_TRUE(tx_status.has_value());
    EXPECT_STATUS(tx_status.value(), ZX_ERR_BAD_STATE);
  }

  ethernet_ifc_protocol_ops_t ifc_ops = {
      .status = [](void* ctx, uint32_t status) {},
      .recv = [](void* ctx, const uint8_t* data_buffer, size_t data_size, uint32_t flags) {},
  };
  ethernet_ifc_protocol_t ifc = {.ops = &ifc_ops, .ctx = nullptr};
  ASSERT_OK(drv->EthernetImplStart(&ifc));

  // Invalid frame lengths (0 and > MTU) must fail immediately with ZX_ERR_INVALID_ARGS.
  for (size_t invalid_len : {size_t{0}, static_cast<size_t>(kTestMtu) + 1}) {
    std::vector<uint8_t> buf;
    std::optional<zx_status_t> status_out;
    drv->EthernetImplQueueTx(0, make_netbuf(buf, invalid_len), tx_cb, &status_out);
    ASSERT_TRUE(status_out.has_value());
    EXPECT_STATUS(status_out.value(), ZX_ERR_INVALID_ARGS);
  }

  // With kTestMtu == kMaxTxBufferSize (32768), tx_request_pool_ has 1 USB TX request.
  // Queue 3 valid TX frames: frame 1 is sent to USB, frame 2 enters pending_tx_queue_ via
  // ZX_ERR_SHOULD_WAIT, and frame 3 enters pending_tx_queue_ via !pending_tx_queue_.is_empty().
  // Also verify that frame 2's completion callback (invoked from UsbWriteComplete while draining
  // pending_tx_queue_) can re-enter EthernetImplQueueTx without deadlocking on mutex_.
  std::vector<uint8_t> tx_buf1, tx_buf2, tx_buf3, reentrant_tx_buf;
  std::optional<zx_status_t> tx_status1, tx_status2, tx_status3, reentrant_tx_status;
  struct ReentrantTxContext {
    usb_cdc_ecm::UsbCdcEcm* drv;
    ethernet_netbuf_t* reentrant_netbuf;
    std::optional<zx_status_t>* status_out;
    std::optional<zx_status_t>* reentrant_status_out;
  } reentrant_ctx = {
      .drv = drv,
      .reentrant_netbuf = make_netbuf(reentrant_tx_buf),
      .status_out = &tx_status2,
      .reentrant_status_out = &reentrant_tx_status,
  };
  auto reentrant_tx_cb = [](void* cookie, zx_status_t status, ethernet_netbuf_t* netbuf) {
    auto* ctx = static_cast<ReentrantTxContext*>(cookie);
    *ctx->status_out = status;
    ctx->drv->EthernetImplQueueTx(
        0, ctx->reentrant_netbuf,
        [](void* inner_cookie, zx_status_t inner_status, ethernet_netbuf_t* inner_netbuf) {
          *static_cast<std::optional<zx_status_t>*>(inner_cookie) = inner_status;
        },
        ctx->reentrant_status_out);
  };
  drv->EthernetImplQueueTx(0, make_netbuf(tx_buf1), tx_cb, &tx_status1);
  drv->EthernetImplQueueTx(0, make_netbuf(tx_buf2), reentrant_tx_cb, &reentrant_ctx);
  drv->EthernetImplQueueTx(0, make_netbuf(tx_buf3), tx_cb, &tx_status3);
  ASSERT_TRUE(tx_status1.has_value());
  EXPECT_OK(tx_status1.value());
  EXPECT_FALSE(tx_status2.has_value());
  EXPECT_FALSE(tx_status3.has_value());
  EXPECT_FALSE(reentrant_tx_status.has_value());
  ASSERT_EQ(QueuedCount(kTestTxEpAddr), 1u);

  // Completing the first USB TX request with ZX_ERR_IO_REFUSED resets the TX endpoint, returns the
  // request to the pool, pops frame 2 from pending_tx_queue_ and sends it, and leaves frame 3 in
  // pending_tx_queue_ because the pool is empty again. Frame 2's completion callback re-enters
  // EthernetImplQueueTx and queues reentrant_tx_buf behind frame 3 without deadlocking.
  {
    auto tx_req = PopRequest(kTestTxEpAddr);
    ASSERT_TRUE(tx_req.has_value());
    usb_request_complete(tx_req->req, ZX_ERR_IO_REFUSED, 0, &tx_req->cb);
  }
  EXPECT_EQ(ResetEndpointCount(kTestTxEpAddr), 1u);
  ASSERT_TRUE(tx_status2.has_value());
  EXPECT_OK(tx_status2.value());
  EXPECT_FALSE(tx_status3.has_value());
  EXPECT_FALSE(reentrant_tx_status.has_value());
  ASSERT_EQ(QueuedCount(kTestTxEpAddr), 1u);

  // Completing the second USB TX request with ZX_ERR_IO_INVALID resets the TX endpoint, increases
  // tx_endpoint_delay_, and pops/sends frame 3.
  {
    auto tx_req = PopRequest(kTestTxEpAddr);
    ASSERT_TRUE(tx_req.has_value());
    usb_request_complete(tx_req->req, ZX_ERR_IO_INVALID, 0, &tx_req->cb);
  }
  EXPECT_EQ(ResetEndpointCount(kTestTxEpAddr), 2u);
  ASSERT_TRUE(tx_status3.has_value());
  EXPECT_OK(tx_status3.value());
  EXPECT_FALSE(reentrant_tx_status.has_value());
  ASSERT_EQ(QueuedCount(kTestTxEpAddr), 1u);

  // EthernetImplStop() must drain the remaining re-entrantly queued frame with ZX_ERR_UNAVAILABLE.
  drv->EthernetImplStop();
  ASSERT_TRUE(reentrant_tx_status.has_value());
  EXPECT_STATUS(reentrant_tx_status.value(), ZX_ERR_UNAVAILABLE);

  // Return the in-flight USB TX request (frame 3) to tx_request_pool_.
  {
    auto tx_req = PopRequest(kTestTxEpAddr);
    ASSERT_TRUE(tx_req.has_value());
    usb_request_complete(tx_req->req, ZX_OK, sizeof(payload), &tx_req->cb);
  }

  // Restart EthernetImpl and queue two TX packets so the second is in pending_tx_queue_ when
  // Stop() is called.
  ASSERT_OK(drv->EthernetImplStart(&ifc));
  std::vector<uint8_t> buf4, buf5;
  std::optional<zx_status_t> tx4_status, tx5_status;
  drv->EthernetImplQueueTx(0, make_netbuf(buf4), tx_cb, &tx4_status);
  ASSERT_TRUE(tx4_status.has_value());
  EXPECT_OK(tx4_status.value());

  drv->EthernetImplQueueTx(0, make_netbuf(buf5), tx_cb, &tx5_status);
  EXPECT_FALSE(tx5_status.has_value());

  ASSERT_OK(driver_test().StopDriver().status_value());
  driver_started_ = false;
  driver_test().runtime().RunUntilIdle();

  // Stop() releases leftover pending_tx_queue_ entries without invoking callbacks owned by an
  // already-unbound client.
  EXPECT_FALSE(tx5_status.has_value());
  EXPECT_EQ(CancelAllCount(kTestTxEpAddr), 1u);

  // QueueTx after Stop() while unbound_ is set must fail immediately with ZX_ERR_IO_NOT_PRESENT.
  std::vector<uint8_t> buf6;
  std::optional<zx_status_t> tx6_status;
  drv->EthernetImplQueueTx(0, make_netbuf(buf6), tx_cb, &tx6_status);
  ASSERT_TRUE(tx6_status.has_value());
  EXPECT_STATUS(tx6_status.value(), ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(UsbCdcEcmDriverTest, InterruptTaskErrorRecoveryAndNotificationEdgeCases) {
  ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);

  // 1. Short interrupt packet (< sizeof(usb_cdc_notification_t)) is ignored and requeued.
  {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    uint8_t short_buf[4] = {0xa1, 0x00, 0x01, 0x00};
    ASSERT_EQ(usb_request_copy_to(int_req->req, short_buf, sizeof(short_buf), 0),
              static_cast<ssize_t>(sizeof(short_buf)));
    usb_request_complete(int_req->req, ZX_OK, sizeof(short_buf), &int_req->cb);
  }
  driver_test().runtime().RunUntilIdle();
  ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);

  // 2. CONNECTION_SPEED_CHANGE with invalid wLength (!= 8) or truncated payload
  // (actual < sizeof(usb_cdc_speed_change_notification_t)) is ignored and requeued.
  struct InvalidSpeedChangeCase {
    uint16_t w_length;
    size_t actual;
  };
  for (const auto& test_case : {
           InvalidSpeedChangeCase{
               .w_length = 4,
               .actual = sizeof(usb_cdc_speed_change_notification_t),
           },
           InvalidSpeedChangeCase{
               .w_length = 8,
               .actual = sizeof(usb_cdc_notification_t),
           },
       }) {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_cdc_speed_change_notification_t bad_speed = {
        .notification =
            {
                .bmRequestType = kClassInterfaceIn,
                .bNotification =
                    fidl::ToUnderlying(fdescriptor::CdcNotification::kConnectionSpeedChange),
                .wValue = 0,
                .wIndex = 0,
                .wLength = test_case.w_length,
            },
        .downlink_br = 987'654'321u,
        .uplink_br = 123'456'789u,
    };
    ASSERT_EQ(usb_request_copy_to(int_req->req, &bad_speed, sizeof(bad_speed), 0),
              static_cast<ssize_t>(sizeof(bad_speed)));
    usb_request_complete(int_req->req, ZX_OK, test_case.actual, &int_req->cb);
    driver_test().runtime().RunUntilIdle();
    EXPECT_EQ(driver_test().driver()->us_bps(), 0u);
    EXPECT_EQ(driver_test().driver()->ds_bps(), 0u);
    ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);
  }

  // 3. Unrecognized notification type is ignored and requeued.
  {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_cdc_notification_t unknown_notif = {
        .bmRequestType = kClassInterfaceIn,
        .bNotification = 0x99,
        .wValue = 0,
        .wIndex = 0,
        .wLength = 0,
    };
    ASSERT_EQ(usb_request_copy_to(int_req->req, &unknown_notif, sizeof(unknown_notif), 0),
              static_cast<ssize_t>(sizeof(unknown_notif)));
    usb_request_complete(int_req->req, ZX_OK, sizeof(unknown_notif), &int_req->cb);
  }
  driver_test().runtime().RunUntilIdle();
  ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);

  // 4. ZX_ERR_IO_REFUSED and ZX_ERR_IO_INVALID reset the interrupt endpoint and reschedule.
  for (zx_status_t err : {ZX_ERR_IO_REFUSED, ZX_ERR_IO_INVALID}) {
    const size_t before_resets = ResetEndpointCount(kTestIntEpAddr);
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_request_complete(int_req->req, err, 0, &int_req->cb);
    driver_test().runtime().RunUntilIdle();
    EXPECT_EQ(ResetEndpointCount(kTestIntEpAddr), before_resets + 1);
    ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);
  }

  // 5. Generic error status (e.g. ZX_ERR_IO) logs and reschedules without resetting the endpoint.
  {
    const size_t before_resets = ResetEndpointCount(kTestIntEpAddr);
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_request_complete(int_req->req, ZX_ERR_IO, 0, &int_req->cb);
    driver_test().runtime().RunUntilIdle();
    EXPECT_EQ(ResetEndpointCount(kTestIntEpAddr), before_resets);
    ASSERT_EQ(QueuedCount(kTestIntEpAddr), 1u);
  }

  // 6. Terminal error status (ZX_ERR_IO_NOT_PRESENT) stops rescheduling the interrupt task.
  {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_request_complete(int_req->req, ZX_ERR_IO_NOT_PRESENT, 0, &int_req->cb);
    driver_test().runtime().RunUntilIdle();
    EXPECT_EQ(QueuedCount(kTestIntEpAddr), 0u);
  }
}

TEST_F(UsbCdcEcmDriverTest, EthernetIfcRxTxCompletionAndPacketFilterPaths) {
  usb_cdc_ecm::UsbCdcEcm* drv = driver_test().driver();

  ethernet_info_t info = {};
  EXPECT_STATUS(drv->EthernetImplQuery(1, &info), ZX_ERR_INVALID_ARGS);
  ASSERT_OK(drv->EthernetImplQuery(0, &info));

  // Verify SetParam for promiscuous mode on/off, ControlOut error propagation, and unsupported
  // parameter handling.
  EXPECT_OK(drv->EthernetImplSetParam(ETHERNET_SETPARAM_PROMISC, 1, nullptr, 0));
  EXPECT_NE(LastPacketFilter() & fdescriptor::kCdcPacketTypePromiscuous, 0);
  EXPECT_OK(drv->EthernetImplSetParam(ETHERNET_SETPARAM_PROMISC, 0, nullptr, 0));
  EXPECT_EQ(LastPacketFilter() & fdescriptor::kCdcPacketTypePromiscuous, 0);

  SetControlOutStatus(ZX_ERR_IO);
  EXPECT_STATUS(drv->EthernetImplSetParam(ETHERNET_SETPARAM_PROMISC, 1, nullptr, 0), ZX_ERR_IO);
  SetControlOutStatus(ZX_OK);
  EXPECT_STATUS(drv->EthernetImplSetParam(0xffffu, 1, nullptr, 0), ZX_ERR_NOT_SUPPORTED);

  struct IfcState {
    std::vector<uint32_t> status_history;
    std::vector<std::vector<uint8_t>> received_frames;
  } ifc_state;

  ethernet_ifc_protocol_ops_t ifc_ops = {
      .status =
          [](void* ctx, uint32_t status) {
            static_cast<IfcState*>(ctx)->status_history.push_back(status);
          },
      .recv =
          [](void* ctx, const uint8_t* data_buffer, size_t data_size, uint32_t flags) {
            static_cast<IfcState*>(ctx)->received_frames.emplace_back(data_buffer,
                                                                      data_buffer + data_size);
          },
  };
  ethernet_ifc_protocol_t ifc = {.ops = &ifc_ops, .ctx = &ifc_state};
  ASSERT_OK(drv->EthernetImplStart(&ifc));
  EXPECT_STATUS(drv->EthernetImplStart(&ifc), ZX_ERR_ALREADY_BOUND);
  ASSERT_EQ(ifc_state.status_history.size(), 1u);
  EXPECT_EQ(ifc_state.status_history.back(), 0u);

  // Transition online via NETWORK_CONNECTION notification.
  auto send_network_connection = [&](uint16_t value) {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_cdc_notification_t notif = {
        .bmRequestType = kClassInterfaceIn,
        .bNotification = fidl::ToUnderlying(fdescriptor::CdcNotification::kNetworkConnection),
        .wValue = value,
        .wIndex = 0,
        .wLength = 0,
    };
    ASSERT_EQ(usb_request_copy_to(int_req->req, &notif, sizeof(notif), 0),
              static_cast<ssize_t>(sizeof(notif)));
    usb_request_complete(int_req->req, ZX_OK, sizeof(notif), &int_req->cb);
    driver_test().runtime().RunUntilIdle();
  };

  send_network_connection(1);
  ASSERT_EQ(ifc_state.status_history.size(), 2u);
  EXPECT_EQ(ifc_state.status_history.back(), ETHERNET_STATUS_ONLINE);
  const size_t rx_queued = QueuedCount(kTestRxEpAddr);
  ASSERT_GT(rx_queued, 0u);

  // Duplicate online notification is a no-op.
  send_network_connection(1);
  EXPECT_EQ(ifc_state.status_history.size(), 2u);

  // Re-binding EthernetImpl while already online immediately reports ETHERNET_STATUS_ONLINE.
  drv->EthernetImplStop();
  ASSERT_OK(drv->EthernetImplStart(&ifc));
  ASSERT_EQ(ifc_state.status_history.size(), 3u);
  EXPECT_EQ(ifc_state.status_history.back(), ETHERNET_STATUS_ONLINE);

  // Exercise UsbReadComplete with ZX_ERR_IO_REFUSED, ZX_ERR_IO_INVALID, ZX_ERR_IO, and ZX_OK.
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    usb_request_complete(rx_req->req, ZX_ERR_IO_REFUSED, 0, &rx_req->cb);
    EXPECT_EQ(ResetEndpointCount(kTestRxEpAddr), 1u);
    EXPECT_EQ(QueuedCount(kTestRxEpAddr), rx_queued);
  }
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    usb_request_complete(rx_req->req, ZX_ERR_IO_INVALID, 0, &rx_req->cb);
    EXPECT_EQ(ResetEndpointCount(kTestRxEpAddr), 2u);
    EXPECT_EQ(QueuedCount(kTestRxEpAddr), rx_queued);
  }
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    usb_request_complete(rx_req->req, ZX_ERR_IO, 0, &rx_req->cb);
    EXPECT_EQ(ResetEndpointCount(kTestRxEpAddr), 2u);
    EXPECT_EQ(QueuedCount(kTestRxEpAddr), rx_queued);
  }
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    const uint8_t frame[] = {0xde, 0xad, 0xbe, 0xef, 0x01, 0x02};
    ASSERT_EQ(usb_request_copy_to(rx_req->req, frame, sizeof(frame), 0),
              static_cast<ssize_t>(sizeof(frame)));
    usb_request_complete(rx_req->req, ZX_OK, sizeof(frame), &rx_req->cb);
    ASSERT_EQ(ifc_state.received_frames.size(), 1u);
    EXPECT_EQ(ifc_state.received_frames[0],
              std::vector<uint8_t>(std::begin(frame), std::end(frame)));
    EXPECT_EQ(QueuedCount(kTestRxEpAddr), rx_queued);
  }
  {
    auto rx_req = PopRequest(kTestRxEpAddr);
    ASSERT_TRUE(rx_req.has_value());
    usb_request_complete(rx_req->req, ZX_ERR_IO_NOT_PRESENT, 0, &rx_req->cb);
    EXPECT_EQ(QueuedCount(kTestRxEpAddr), rx_queued - 1);
  }

  // Transition offline via NETWORK_CONNECTION (wValue = 0), and verify duplicate offline is a
  // no-op.
  send_network_connection(0);
  ASSERT_EQ(ifc_state.status_history.size(), 4u);
  EXPECT_EQ(ifc_state.status_history.back(), 0u);
  send_network_connection(0);
  EXPECT_EQ(ifc_state.status_history.size(), 4u);
}

TEST_F(UsbCdcEcmDriverTest, StopWaitsForInFlightUsbReadComplete) {
  usb_cdc_ecm::UsbCdcEcm* drv = driver_test().driver();

  struct BlockingRecvState {
    sync_completion_t entered;
    sync_completion_t allow_exit;
    std::atomic<bool> exited{false};
  } recv_state;

  ethernet_ifc_protocol_ops_t ifc_ops = {
      .status = [](void* ctx, uint32_t status) {},
      .recv =
          [](void* ctx, const uint8_t* data_buffer, size_t data_size, uint32_t flags) {
            auto* state = static_cast<BlockingRecvState*>(ctx);
            sync_completion_signal(&state->entered);
            sync_completion_wait(&state->allow_exit, ZX_TIME_INFINITE);
            state->exited.store(true, std::memory_order_release);
          },
  };
  ethernet_ifc_protocol_t ifc = {.ops = &ifc_ops, .ctx = &recv_state};
  ASSERT_OK(drv->EthernetImplStart(&ifc));

  // Go online so RX requests are queued.
  {
    auto int_req = PopRequest(kTestIntEpAddr);
    ASSERT_TRUE(int_req.has_value());
    usb_cdc_notification_t notif = {
        .bmRequestType = kClassInterfaceIn,
        .bNotification = fidl::ToUnderlying(fdescriptor::CdcNotification::kNetworkConnection),
        .wValue = 1,
        .wIndex = 0,
        .wLength = 0,
    };
    ASSERT_EQ(usb_request_copy_to(int_req->req, &notif, sizeof(notif), 0),
              static_cast<ssize_t>(sizeof(notif)));
    usb_request_complete(int_req->req, ZX_OK, sizeof(notif), &int_req->cb);
  }
  driver_test().runtime().RunUntilIdle();
  ASSERT_GT(QueuedCount(kTestRxEpAddr), 0u);

  auto rx_req = PopRequest(kTestRxEpAddr);
  ASSERT_TRUE(rx_req.has_value());
  const uint8_t frame[] = {0x01, 0x02, 0x03, 0x04};
  ASSERT_EQ(usb_request_copy_to(rx_req->req, frame, sizeof(frame), 0),
            static_cast<ssize_t>(sizeof(frame)));

  std::thread rx_thread(
      [&]() { usb_request_complete(rx_req->req, ZX_OK, sizeof(frame), &rx_req->cb); });
  auto join_rx_thread = fit::defer([&]() {
    SetOnCancelAll(nullptr);
    sync_completion_signal(&recv_state.allow_exit);
    if (rx_thread.joinable()) {
      rx_thread.join();
    }
  });

  sync_completion_wait(&recv_state.entered, ZX_TIME_INFINITE);

  // Post allow_exit onto the driver's synchronized dispatcher when Stop() cancels the last
  // endpoint (tx_endpoint_). Because Stop() is currently running on that dispatcher, the posted
  // task cannot execute until Stop() has observed in_flight_requests_ > 0, stored stop_completer_,
  // and returned.
  bool posted_allow_exit = false;
  SetOnCancelAll([&](uint8_t ep_addr) {
    if (ep_addr == kTestTxEpAddr) {
      async::PostTask(fdf::Dispatcher::GetCurrent()->async_dispatcher(), [&]() {
        posted_allow_exit = true;
        sync_completion_signal(&recv_state.allow_exit);
      });
    }
  });

  ASSERT_OK(driver_test().StopDriver().status_value());
  driver_started_ = false;
  SetOnCancelAll(nullptr);
  EXPECT_TRUE(posted_allow_exit);
  EXPECT_TRUE(recv_state.exited.load(std::memory_order_acquire));
  EXPECT_EQ(QueuedCount(kTestRxEpAddr), 0u);

  rx_thread.join();
}

TEST(UsbCdcEcmStartTest, StartWithIadSkipsClaimingAdditionalInterfaces) {
  fdf_testing::ForegroundDriverTest<UsbCdcEcmFixtureConfig> driver_test;

  std::vector<uint8_t> iad_descriptors;
  usb_interface_assoc_descriptor_t iad = {
      .b_length = sizeof(usb_interface_assoc_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
      .b_first_interface = 0,
      .b_interface_count = 2,
      .b_function_class = fidl::ToUnderlying(fdescriptor::UsbClass::kComm),
      .b_function_sub_class = fidl::ToUnderlying(fdescriptor::CdcSubclass::kEthernet),
      .b_function_protocol = 0,
      .i_function = 0,
  };
  const auto* iad_bytes = reinterpret_cast<const uint8_t*>(&iad);
  iad_descriptors.insert(iad_descriptors.end(), iad_bytes, iad_bytes + sizeof(iad));
  std::vector<uint8_t> base_descriptors = BuildValidCdcEcmDescriptors();
  iad_descriptors.insert(iad_descriptors.end(), base_descriptors.begin(), base_descriptors.end());

  // Simulate a sibling CDC data interface belonging to a subsequent composite function; Start()
  // must not claim it when the CDC-ECM data interface is already part of usb_'s descriptors.
  usb_interface_descriptor_t sibling_cdc_ifc = {
      .b_length = sizeof(usb_interface_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
      .b_interface_number = 2,
      .b_alternate_setting = 0,
      .b_num_endpoints = 0,
      .b_interface_class = fidl::ToUnderlying(fdescriptor::UsbClass::kCdc),
      .b_interface_sub_class = 0,
      .b_interface_protocol = 0,
      .i_interface = 0,
  };
  const auto* sibling_bytes = reinterpret_cast<const uint8_t*>(&sibling_cdc_ifc);
  std::vector<uint8_t> additional(sibling_bytes, sibling_bytes + sizeof(sibling_cdc_ifc));

  driver_test.RunInEnvironmentTypeContext([&](UsbCdcEcmTestEnvironment& env) {
    env.usb_server().set_descriptors(std::move(iad_descriptors));
    env.usb_server().set_additional_descriptors(std::move(additional));
  });

  ASSERT_OK(driver_test.StartDriver().status_value());
  size_t claimed = 0;
  driver_test.RunInEnvironmentTypeContext(
      [&](UsbCdcEcmTestEnvironment& env) { claimed = env.usb_server().claimed_interface_count(); });
  EXPECT_EQ(claimed, 0u);
  ASSERT_OK(driver_test.StopDriver().status_value());
}

TEST(UsbCdcEcmStartTest, StartWithoutIadClaimsAdditionalDataInterfaces) {
  fdf_testing::ForegroundDriverTest<UsbCdcEcmFixtureConfig> driver_test;

  driver_test.RunInEnvironmentTypeContext([&](UsbCdcEcmTestEnvironment& env) {
    env.usb_server().set_descriptors(BuildCommDescriptors());
    env.usb_server().set_additional_descriptors(BuildDataDescriptors());
  });

  ASSERT_OK(driver_test.StartDriver().status_value());
  size_t claimed = 0;
  driver_test.RunInEnvironmentTypeContext(
      [&](UsbCdcEcmTestEnvironment& env) { claimed = env.usb_server().claimed_interface_count(); });
  EXPECT_EQ(claimed, 1u);
  ASSERT_OK(driver_test.StopDriver().status_value());
}

}  // namespace
