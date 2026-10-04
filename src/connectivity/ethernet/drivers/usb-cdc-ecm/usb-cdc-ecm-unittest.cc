// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/testing/cpp/scoped_global_logger.h>

#include <vector>

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

}  // namespace
