// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <vector>

#include <usb/usb.h>
#include <zxtest/zxtest.h>

#include "lib/fit/defer.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace {

constexpr usb_descriptor_header_t kTestDescriptorHeader = {
    .b_length = sizeof(usb_descriptor_header_t),
    .b_descriptor_type = 0,
};

constexpr usb_interface_descriptor_t kTestUsbInterfaceDescriptor = {
    .b_length = sizeof(usb_interface_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
    .b_interface_number = 0,
    .b_alternate_setting = 0,
    .b_num_endpoints = 2,
    .b_interface_class = 8,
    .b_interface_sub_class = 6,
    .b_interface_protocol = 80,
    .i_interface = 0,
};

constexpr usb_endpoint_descriptor_t kTestUsbEndpointDescriptor = {
    .b_length = sizeof(usb_endpoint_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
    .b_endpoint_address = 0x81,
    .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
    .w_max_packet_size = 1024,
    .b_interval = 0,
};

constexpr usb_ss_ep_comp_descriptor_t kTestUsbSsEpCompDescriptor = {
    .b_length = sizeof(usb_ss_ep_comp_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
    .b_max_burst = 3,
    .bm_attributes = 0,
    .w_bytes_per_interval = 0,
};

constexpr usb_ss_isoch_ep_comp_descriptor_t kTestUsbSsIsochEpCompDescriptor = {
    .b_length = sizeof(usb_ss_isoch_ep_comp_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsIsochEpCompanion),
    .w_reserved = 0,
    .dw_bytes_per_interval = 1024,
};

constexpr usb_interface_assoc_descriptor_t kTestUsbInterfaceAssocDescriptor = {
    .b_length = sizeof(usb_interface_assoc_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
    .b_first_interface = 0,
    .b_interface_count = 2,
    .b_function_class = 8,
    .b_function_sub_class = 6,
    .b_function_protocol = 80,
    .i_function = 0,
};

void ExpectHeaderEq(const usb_descriptor_header_t* actual,
                    const usb_descriptor_header_t& expected) {
  ASSERT_NE(nullptr, actual);
  ASSERT_BYTES_EQ(actual, &expected, sizeof(expected));
}

void ExpectInterfaceEq(const usb_interface_descriptor_t* actual,
                       const usb_interface_descriptor_t& expected) {
  ASSERT_NE(nullptr, actual);
  ASSERT_BYTES_EQ(actual, &expected, sizeof(expected));
}

void ExpectEndpointEq(const usb_endpoint_descriptor_t* actual,
                      const usb_endpoint_descriptor_t& expected) {
  ASSERT_NE(nullptr, actual);
  ASSERT_BYTES_EQ(actual, &expected, sizeof(expected));
}

void ExpectSsEpCompEq(const usb_ss_ep_comp_descriptor_t* actual,
                      const usb_ss_ep_comp_descriptor_t& expected) {
  ASSERT_NE(nullptr, actual);
  ASSERT_BYTES_EQ(actual, &expected, sizeof(expected));
}

void ExpectSsIsochEpCompEq(const usb_ss_isoch_ep_comp_descriptor_t* actual,
                           const usb_ss_isoch_ep_comp_descriptor_t& expected) {
  ASSERT_NE(nullptr, actual);
  ASSERT_BYTES_EQ(actual, &expected, sizeof(expected));
}

class UsbLibTest : public zxtest::Test {
 public:
  void SetUp() override {
    proto_.ops = &ops_;
    proto_.ctx = this;
    ops_.get_descriptors_length = UsbGetDescriptorsLength;
    ops_.get_descriptors = UsbGetDescriptors;
  }

 protected:
  static void UsbGetDescriptors(void* ctx, uint8_t* out_descs_buffer, size_t descs_size,
                                size_t* out_descs_actual) {
    auto test = reinterpret_cast<UsbLibTest*>(ctx);
    size_t descriptors_length = test->GetDescriptorLength();
    if (descs_size < descriptors_length) {
      descriptors_length = descs_size;
    }
    memcpy(out_descs_buffer, test->GetDescriptors(), descriptors_length);
    *out_descs_actual = descriptors_length;
  }

  static size_t UsbGetDescriptorsLength(void* ctx) {
    auto test = reinterpret_cast<UsbLibTest*>(ctx);
    return test->GetDescriptorLength();
  }

  void SetDescriptors(void* descriptors) { descriptors_ = descriptors; }

  void* GetDescriptors() { return descriptors_; }

  void SetDescriptorLength(size_t descriptor_length) { descriptor_length_ = descriptor_length; }

  size_t GetDescriptorLength() { return descriptor_length_; }

  usb_protocol_t* GetUsbProto() { return &proto_; }

  usb_protocol_t proto_{};
  usb_protocol_ops_t ops_{};
  void* descriptors_ = nullptr;
  size_t descriptor_length_ = 0;
};

TEST_F(UsbLibTest, TestUsbDescIterPeekNormal) {
  usb_desc_iter_t iter;
  SetDescriptors((void*)&kTestDescriptorHeader);
  SetDescriptorLength(sizeof(kTestDescriptorHeader));
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto desc = usb_desc_iter_peek(&iter);
  ExpectHeaderEq(desc, kTestDescriptorHeader);
  usb_desc_iter_release(&iter);
}

TEST_F(UsbLibTest, TestUsbDescPeekOverflow) {
  usb_desc_iter_t iter;
  usb_descriptor_header_t desc = kTestDescriptorHeader;
  // Length is invalid and longer than the actual length.
  desc.b_length++;
  SetDescriptors((void*)&desc);
  SetDescriptorLength(sizeof(desc));
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  ASSERT_EQ(nullptr, usb_desc_iter_peek(&iter));
  usb_desc_iter_release(&iter);
}

TEST_F(UsbLibTest, TestUsbDescIterPeekHeaderTooShort) {
  usb_desc_iter_t iter;
  SetDescriptors((void*)&kTestDescriptorHeader);
  SetDescriptorLength(sizeof(kTestDescriptorHeader) - 1);
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  ASSERT_EQ(nullptr, usb_desc_iter_peek(&iter));
  usb_desc_iter_release(&iter);
}

TEST_F(UsbLibTest, TestUsbDescClone) {
  usb_desc_iter_t src;
  SetDescriptors((void*)&kTestDescriptorHeader);
  SetDescriptorLength(sizeof(kTestDescriptorHeader));
  auto status = usb_desc_iter_init(GetUsbProto(), &src);
  ASSERT_OK(status);
  usb_desc_iter_t dest;
  ASSERT_OK(usb_desc_iter_clone(&src, &dest));
  // This should not affect dest.
  usb_desc_iter_release(&src);
  auto desc = usb_desc_iter_peek(&dest);
  ExpectHeaderEq(desc, kTestDescriptorHeader);
  ASSERT_TRUE(usb_desc_iter_advance(&dest));
  ASSERT_EQ(nullptr, usb_desc_iter_peek(&dest));
  usb_desc_iter_release(&dest);
}

TEST_F(UsbLibTest, TestUsbDescAdvanceReset) {
  usb_desc_iter_t iter;
  SetDescriptors((void*)&kTestDescriptorHeader);
  SetDescriptorLength(sizeof(kTestDescriptorHeader));
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  ASSERT_TRUE(usb_desc_iter_advance(&iter));
  ASSERT_FALSE(usb_desc_iter_advance(&iter));
  usb_desc_iter_reset(&iter);
  auto desc = usb_desc_iter_peek(&iter);
  ASSERT_TRUE(usb_desc_iter_advance(&iter));
  ExpectHeaderEq(desc, kTestDescriptorHeader);
  ASSERT_EQ(nullptr, usb_desc_iter_peek(&iter));
  usb_desc_iter_release(&iter);
}

TEST_F(UsbLibTest, TestUsbDescGetStructureNormal) {
  usb_desc_iter_t iter;
  SetDescriptors((void*)&kTestUsbInterfaceDescriptor);
  SetDescriptorLength(sizeof(kTestUsbInterfaceDescriptor));
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto desc = usb_desc_iter_get_structure(&iter, sizeof(kTestUsbInterfaceDescriptor));
  ExpectInterfaceEq(reinterpret_cast<const usb_interface_descriptor_t*>(desc),
                    kTestUsbInterfaceDescriptor);
  ASSERT_TRUE(usb_desc_iter_advance(&iter));
  ASSERT_EQ(nullptr, usb_desc_iter_get_structure(&iter, sizeof(kTestUsbInterfaceDescriptor)));
  usb_desc_iter_release(&iter);
}

TEST_F(UsbLibTest, TestUsbDescGetStructureOverflow) {
  usb_desc_iter_t iter;
  usb_interface_descriptor_t desc = kTestUsbInterfaceDescriptor;
  SetDescriptors((void*)&desc);
  SetDescriptorLength(sizeof(desc) - 1);
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  ASSERT_EQ(nullptr, usb_desc_iter_get_structure(&iter, sizeof(kTestUsbInterfaceDescriptor)));
  usb_desc_iter_release(&iter);
}

TEST_F(UsbLibTest, TestUsbDescIterNextInterface) {
  // Layout is | Intf | Ep | SsEp | Intf | Ep | SsEp |.
  size_t desc_length = (sizeof(kTestUsbInterfaceDescriptor) + sizeof(kTestUsbEndpointDescriptor) +
                        sizeof(kTestUsbSsEpCompDescriptor)) *
                       2;
  std::vector<uint8_t> desc(desc_length);
  uint8_t* ptr = desc.data();
  usb_desc_iter_t iter;
  for (size_t i = 0; i < 2; i++) {
    memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
    ptr += sizeof(kTestUsbInterfaceDescriptor);
    memcpy(ptr, &kTestUsbEndpointDescriptor, sizeof(kTestUsbEndpointDescriptor));
    ptr += sizeof(kTestUsbEndpointDescriptor);
    memcpy(ptr, &kTestUsbSsEpCompDescriptor, sizeof(kTestUsbSsEpCompDescriptor));
    ptr += sizeof(kTestUsbSsEpCompDescriptor);
  }
  SetDescriptors(desc.data());
  SetDescriptorLength(desc_length);
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto iter_cleanup = fit::defer([&iter]() { usb_desc_iter_release(&iter); });
  for (size_t i = 0; i < 2; i++) {
    usb_interface_descriptor_t* interface = usb_desc_iter_next_interface(&iter, false);
    ExpectInterfaceEq(interface, kTestUsbInterfaceDescriptor);
  }
  ASSERT_EQ(nullptr, usb_desc_iter_next_interface(&iter, false));
}

TEST_F(UsbLibTest, TestUsbDescIterNextEndpoint) {
  // Layout is | Intf | Ep | Ep | Intf |.
  size_t desc_length =
      sizeof(kTestUsbInterfaceDescriptor) * 2 + sizeof(kTestUsbEndpointDescriptor) * 2;
  std::vector<uint8_t> desc(desc_length);
  uint8_t* ptr = desc.data();
  usb_desc_iter_t iter;
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  for (size_t i = 0; i < 2; i++) {
    memcpy(ptr, &kTestUsbEndpointDescriptor, sizeof(kTestUsbEndpointDescriptor));
    ptr += sizeof(kTestUsbEndpointDescriptor);
  }
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  SetDescriptors(desc.data());
  SetDescriptorLength(desc_length);
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto iter_cleanup = fit::defer([&iter]() { usb_desc_iter_release(&iter); });
  ASSERT_NE(nullptr, usb_desc_iter_next_interface(&iter, false));
  for (size_t i = 0; i < 2; i++) {
    usb_endpoint_descriptor_t* ep = usb_desc_iter_next_endpoint(&iter);
    ExpectEndpointEq(ep, kTestUsbEndpointDescriptor);
  }
  ASSERT_EQ(nullptr, usb_desc_iter_next_endpoint(&iter));
}

TEST_F(UsbLibTest, TestUsbDescIterNextEndpointIadBoundary) {
  // Layout is | Intf | Ep | IAD | Intf |.
  size_t desc_length = sizeof(kTestUsbInterfaceDescriptor) * 2 +
                       sizeof(kTestUsbEndpointDescriptor) +
                       sizeof(kTestUsbInterfaceAssocDescriptor);
  std::vector<uint8_t> desc(desc_length);
  uint8_t* ptr = desc.data();
  usb_desc_iter_t iter;
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  memcpy(ptr, &kTestUsbEndpointDescriptor, sizeof(kTestUsbEndpointDescriptor));
  ptr += sizeof(kTestUsbEndpointDescriptor);
  memcpy(ptr, &kTestUsbInterfaceAssocDescriptor, sizeof(kTestUsbInterfaceAssocDescriptor));
  ptr += sizeof(kTestUsbInterfaceAssocDescriptor);
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  SetDescriptors(desc.data());
  SetDescriptorLength(desc_length);
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto iter_cleanup = fit::defer([&iter]() { usb_desc_iter_release(&iter); });
  ASSERT_NE(nullptr, usb_desc_iter_next_interface(&iter, false));
  usb_endpoint_descriptor_t* ep = usb_desc_iter_next_endpoint(&iter);
  ExpectEndpointEq(ep, kTestUsbEndpointDescriptor);
  ASSERT_EQ(nullptr, usb_desc_iter_next_endpoint(&iter));
}

TEST_F(UsbLibTest, TestTruncatedInterfaceAssociationDescriptor) {
  uint8_t desc[] = {
      // Truncated IAD (length 4 instead of 8)
      4,
      fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
      0,
      0,
  };
  usb_desc_iter_t iter;
  ASSERT_OK(usb_desc_iter_init_unowned(desc, sizeof(desc), &iter));
  usb_interface_assoc_descriptor_t* assoc = nullptr;
  EXPECT_EQ(nullptr, usb_desc_iter_next_interface_with_assoc(&iter, false, &assoc));
  EXPECT_EQ(nullptr, assoc);
  ASSERT_OK(usb_desc_iter_init_unowned(desc, sizeof(desc), &iter));
  EXPECT_EQ(nullptr, usb_desc_iter_next_interface(&iter, false));
}

TEST_F(UsbLibTest, TestTruncatedDescriptorHeaderOneByte) {
  uint8_t desc[] = {
      // Truncated header (length 1 instead of >= 2)
      1,
  };
  usb_desc_iter_t iter;
  ASSERT_OK(usb_desc_iter_init_unowned(desc, sizeof(desc), &iter));
  EXPECT_EQ(nullptr, usb_desc_iter_peek(&iter));
  EXPECT_EQ(nullptr, usb_desc_iter_next_interface(&iter, false));
}

TEST_F(UsbLibTest, TestMultipleInterfaceAssociationDescriptors) {
  struct {
    usb_interface_assoc_descriptor_t iad1;
    usb_interface_assoc_descriptor_t iad2;
    usb_interface_descriptor_t intf;
  } desc = {
      .iad1 =
          {
              .b_length = sizeof(usb_interface_assoc_descriptor_t),
              .b_descriptor_type =
                  fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
              .b_first_interface = 0,
              .b_interface_count = 2,
              .b_function_class = 1,
              .b_function_sub_class = 1,
              .b_function_protocol = 0,
              .i_function = 0,
          },
      .iad2 =
          {
              .b_length = sizeof(usb_interface_assoc_descriptor_t),
              .b_descriptor_type =
                  fidl::ToUnderlying(fdescriptor::DescriptorType::kInterfaceAssociation),
              .b_first_interface = 2,
              .b_interface_count = 2,
              .b_function_class = 2,
              .b_function_sub_class = 2,
              .b_function_protocol = 0,
              .i_function = 0,
          },
      .intf =
          {
              .b_length = sizeof(usb_interface_descriptor_t),
              .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
              .b_interface_number = 0,
              .b_alternate_setting = 0,
              .b_num_endpoints = 0,
              .b_interface_class = 1,
              .b_interface_sub_class = 1,
              .b_interface_protocol = 0,
              .i_interface = 0,
          },
  };
  usb_desc_iter_t iter;
  ASSERT_OK(usb_desc_iter_init_unowned(&desc, sizeof(desc), &iter));
  usb_interface_assoc_descriptor_t* assoc = nullptr;
  auto* intf = usb_desc_iter_next_interface_with_assoc(&iter, false, &assoc);
  ASSERT_NE(nullptr, intf);
  EXPECT_EQ(0, intf->b_interface_number);
  ASSERT_NE(nullptr, assoc);
  // *assoc strictly binds to the immediately preceding (first seen before interface) IAD
  EXPECT_EQ(0, assoc->b_first_interface);
}

TEST_F(UsbLibTest, TestUsbDescIterNextSsEpComp) {
  // Layout is | Intf | Ep | SsEp | SsEp | Intf |.
  size_t desc_length = sizeof(kTestUsbInterfaceDescriptor) * 2 +
                       sizeof(kTestUsbEndpointDescriptor) + sizeof(kTestUsbSsEpCompDescriptor) * 2;
  std::vector<uint8_t> desc(desc_length);
  uint8_t* ptr = desc.data();
  usb_desc_iter_t iter;
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  memcpy(ptr, &kTestUsbEndpointDescriptor, sizeof(kTestUsbEndpointDescriptor));
  ptr += sizeof(kTestUsbEndpointDescriptor);
  memcpy(ptr, &kTestUsbSsEpCompDescriptor, sizeof(kTestUsbSsEpCompDescriptor));
  ptr += sizeof(kTestUsbSsEpCompDescriptor);
  memcpy(ptr, &kTestUsbSsEpCompDescriptor, sizeof(kTestUsbSsEpCompDescriptor));
  ptr += sizeof(kTestUsbSsEpCompDescriptor);
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  SetDescriptors(desc.data());
  SetDescriptorLength(desc_length);
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto iter_cleanup = fit::defer([&iter]() { usb_desc_iter_release(&iter); });
  ASSERT_NE(nullptr, usb_desc_iter_next_interface(&iter, false));
  ASSERT_NE(nullptr, usb_desc_iter_next_endpoint(&iter));
  for (size_t i = 0; i < 2; i++) {
    usb_ss_ep_comp_descriptor_t* ss_ep = usb_desc_iter_next_ss_ep_comp(&iter);
    ExpectSsEpCompEq(ss_ep, kTestUsbSsEpCompDescriptor);
  }
  ASSERT_EQ(nullptr, usb_desc_iter_next_ss_ep_comp(&iter));
}

TEST(UsbDescriptorsTest, WValueHelpers) {
  constexpr uint16_t kDeviceWValue = usb_descriptor_w_value(fdescriptor::DescriptorType::kDevice);
  EXPECT_EQ(kDeviceWValue, 0x0100u);
  EXPECT_EQ(usb_descriptor_type_from_w_value(kDeviceWValue), fdescriptor::DescriptorType::kDevice);
  EXPECT_EQ(usb_descriptor_index_from_w_value(kDeviceWValue), 0u);

  constexpr uint16_t kStringWValue =
      usb_descriptor_w_value(fdescriptor::DescriptorType::kString, 5);
  EXPECT_EQ(kStringWValue, 0x0305u);
  EXPECT_EQ(usb_descriptor_type_from_w_value(kStringWValue), fdescriptor::DescriptorType::kString);
  EXPECT_EQ(usb_descriptor_index_from_w_value(kStringWValue), 5u);

  constexpr uint16_t kRawTypeWValue = usb_descriptor_w_value(static_cast<uint8_t>(0x21), 2);
  EXPECT_EQ(kRawTypeWValue, 0x2102u);
  EXPECT_EQ(usb_descriptor_type_from_w_value(kRawTypeWValue), fdescriptor::DescriptorType::kHid);
  EXPECT_EQ(usb_descriptor_index_from_w_value(kRawTypeWValue), 2u);
}

TEST(UsbDescriptorsTest, BmRequestTypeHelpersAndOperators) {
  // Bitwise OR (enum | enum, enum | integral, integral | enum).
  constexpr uint8_t kStdDevIn = fdescriptor::EndpointDirection::kIn |
                                fdescriptor::RequestType::kStandard |
                                fdescriptor::RequestRecipient::kDevice;
  EXPECT_EQ(kStdDevIn, kStandardDeviceIn);
  EXPECT_EQ(kStdDevIn, fdescriptor::kStandardDeviceRequestIn);
  EXPECT_TRUE(usb_request_is_in(kStdDevIn));
  EXPECT_FALSE(usb_request_is_out(kStdDevIn));
  EXPECT_EQ(usb_request_type(kStdDevIn), fdescriptor::RequestType::kStandard);

  constexpr uint8_t kClsIfOut =
      static_cast<uint8_t>(0) |
      (fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kClass) |
      fdescriptor::RequestRecipient::kInterface;
  EXPECT_EQ(kClsIfOut, kClassInterfaceOut);
  EXPECT_FALSE(usb_request_is_in(kClsIfOut));
  EXPECT_TRUE(usb_request_is_out(kClsIfOut));
  EXPECT_EQ(usb_request_type(kClsIfOut), fdescriptor::RequestType::kClass);

  constexpr uint8_t kVendorDevIn = fdescriptor::EndpointDirection::kIn |
                                   fdescriptor::RequestType::kVendor |
                                   fdescriptor::RequestRecipient::kDevice;
  EXPECT_EQ(kVendorDevIn, kVendorDeviceIn);
  EXPECT_TRUE(usb_request_is_in(kVendorDevIn));
  EXPECT_EQ(usb_request_type(kVendorDevIn), fdescriptor::RequestType::kVendor);

  // Bitwise AND (enum & enum, enum & integral, integral & enum).
  EXPECT_EQ(fdescriptor::EndpointDirection::kIn & fdescriptor::EndpointDirection::kOut, 0u);
  EXPECT_EQ(fdescriptor::EndpointDirection::kIn & fdescriptor::kEndpointDirectionMask, 0x80u);
  EXPECT_EQ(kClsIfOut & fdescriptor::RequestType::kClass, 0x20u);
  EXPECT_EQ(kClsIfOut & fdescriptor::RequestRecipient::kInterface, 0x01u);

  // Equality operators (integral == enum, enum == integral).
  EXPECT_TRUE(0x01u == fdescriptor::DescriptorType::kDevice);
  EXPECT_TRUE(fdescriptor::DescriptorType::kDevice == 0x01u);
  EXPECT_FALSE(0x02u == fdescriptor::DescriptorType::kDevice);
  EXPECT_TRUE(0x80u == fdescriptor::EndpointDirection::kIn);
  EXPECT_TRUE(fdescriptor::RequestType::kClass == 0x20);
  EXPECT_TRUE(fdescriptor::UsbClass::kHid == 0x03u);
  EXPECT_TRUE(0x06u == fdescriptor::StandardRequest::kGetDescriptor);
}

TEST(UsbDescriptorsTest, EndpointNumberMaskAndHelpers) {
  // USB 2.0 section 9.6.6 defines bits 3:0 of bEndpointAddress as the endpoint number and
  // bits 6:4 as reserved (reset to zero). Verify usb_ep_num masks out bits 4-6 as well as bit 7.
  EXPECT_EQ(usb_ep_num(0x15u), 5u);
  EXPECT_EQ(usb_ep_num(0x9fu), 15u);
  EXPECT_EQ(usb_ep_num(0x7fu), 15u);
  EXPECT_EQ(usb_ep_num(0x80u), 0u);

  usb_endpoint_descriptor_t c_ep = {
      .b_length = sizeof(usb_endpoint_descriptor_t),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = 0x95,  // IN + reserved bit 4 set + EP 5
      .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kIsochronous) |
                       (fidl::ToUnderlying(fdescriptor::SynchronizationType::kAdaptive) << 2),
      .w_max_packet_size = htole16(0x1400),  // 1024 bytes + bits 12:11 set
      .b_interval = 1,
  };
  EXPECT_EQ(usb_ep_num(c_ep), 5u);
  EXPECT_EQ(usb_ep_num(&c_ep), 5u);
  EXPECT_EQ(usb_ep_direction(c_ep), fdescriptor::EndpointDirection::kIn);
  EXPECT_TRUE(usb_ep_is_in(c_ep));
  EXPECT_FALSE(usb_ep_is_out(&c_ep));
  EXPECT_EQ(usb_ep_type(c_ep), fdescriptor::EndpointType::kIsochronous);
  EXPECT_TRUE(usb_ep_is_isoch(&c_ep));
  EXPECT_FALSE(usb_ep_is_bulk(c_ep));
  EXPECT_EQ(usb_ep_sync_type(c_ep), fdescriptor::SynchronizationType::kAdaptive);
  EXPECT_EQ(usb_ep_max_packet(c_ep), 1024u);
  EXPECT_EQ(usb_ep_max_packet(&c_ep), 1024u);

  fdescriptor::UsbEndpointDescriptor fidl_ep(
      sizeof(usb_endpoint_descriptor_t), fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      0x15, fidl::ToUnderlying(fdescriptor::EndpointType::kBulk), 512, 0);
  EXPECT_EQ(usb_ep_num(fidl_ep), 5u);
  EXPECT_TRUE(usb_ep_is_out(fidl_ep));
  EXPECT_TRUE(usb_ep_is_bulk(fidl_ep));
  EXPECT_EQ(usb_ep_max_packet(fidl_ep), 512u);

  struct UnsupportedType {};
  static_assert(!fuchsia_hardware_usb_descriptor_internal::HasEpAddress<UnsupportedType>);
  static_assert(!fuchsia_hardware_usb_descriptor_internal::HasEpAttributes<UnsupportedType>);
  static_assert(!fuchsia_hardware_usb_descriptor_internal::HasEpMaxPacketSize<UnsupportedType>);
  static_assert(!fuchsia_hardware_usb_descriptor_internal::HasEpMaxPacketSize<uint8_t>);
}

TEST_F(UsbLibTest, TestUsbDescIterNextSsIsochEpComp) {
  // Layout is | Intf | Ep | SsEp | SsIsochEp | Intf |.
  size_t desc_length = sizeof(kTestUsbInterfaceDescriptor) * 2 +
                       sizeof(kTestUsbEndpointDescriptor) + sizeof(kTestUsbSsEpCompDescriptor) +
                       sizeof(kTestUsbSsIsochEpCompDescriptor);
  std::vector<uint8_t> desc(desc_length);
  uint8_t* ptr = desc.data();
  usb_desc_iter_t iter;
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  memcpy(ptr, &kTestUsbEndpointDescriptor, sizeof(kTestUsbEndpointDescriptor));
  ptr += sizeof(kTestUsbEndpointDescriptor);
  memcpy(ptr, &kTestUsbSsEpCompDescriptor, sizeof(kTestUsbSsEpCompDescriptor));
  ptr += sizeof(kTestUsbSsEpCompDescriptor);
  memcpy(ptr, &kTestUsbSsIsochEpCompDescriptor, sizeof(kTestUsbSsIsochEpCompDescriptor));
  ptr += sizeof(kTestUsbSsIsochEpCompDescriptor);
  memcpy(ptr, &kTestUsbInterfaceDescriptor, sizeof(kTestUsbInterfaceDescriptor));
  ptr += sizeof(kTestUsbInterfaceDescriptor);
  SetDescriptors(desc.data());
  SetDescriptorLength(desc_length);

  // Case 1: Calling next_ss_isoch_ep_comp after next_endpoint directly (advances past SsEpComp).
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter));
  auto iter_cleanup1 = fit::defer([&iter]() { usb_desc_iter_release(&iter); });
  ASSERT_NE(nullptr, usb_desc_iter_next_interface(&iter, false));
  ASSERT_NE(nullptr, usb_desc_iter_next_endpoint(&iter));
  usb_ss_isoch_ep_comp_descriptor_t* isoch_ep = usb_desc_iter_next_ss_isoch_ep_comp(&iter);
  ExpectSsIsochEpCompEq(isoch_ep, kTestUsbSsIsochEpCompDescriptor);
  ASSERT_EQ(nullptr, usb_desc_iter_next_ss_isoch_ep_comp(&iter));

  // Case 2: Calling next_ss_isoch_ep_comp after next_ss_ep_comp.
  usb_desc_iter_t iter2;
  ASSERT_OK(usb_desc_iter_init(GetUsbProto(), &iter2));
  auto iter_cleanup2 = fit::defer([&iter2]() { usb_desc_iter_release(&iter2); });
  ASSERT_NE(nullptr, usb_desc_iter_next_interface(&iter2, false));
  ASSERT_NE(nullptr, usb_desc_iter_next_endpoint(&iter2));
  ASSERT_NE(nullptr, usb_desc_iter_next_ss_ep_comp(&iter2));
  isoch_ep = usb_desc_iter_next_ss_isoch_ep_comp(&iter2);
  ExpectSsIsochEpCompEq(isoch_ep, kTestUsbSsIsochEpCompDescriptor);
  ASSERT_EQ(nullptr, usb_desc_iter_next_ss_isoch_ep_comp(&iter2));
}

}  // namespace
