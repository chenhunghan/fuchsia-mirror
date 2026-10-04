// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_TESTING_DESCRIPTOR_BUILDER_DESCRIPTOR_BUILDER_H_
#define SRC_DEVICES_USB_TESTING_DESCRIPTOR_BUILDER_DESCRIPTOR_BUILDER_H_
#include <cstdint>
#include <vector>

#include <usb/descriptors.h>
#include <usb/usb.h>

namespace usb {
namespace fdescriptor = fuchsia_hardware_usb_descriptor;
static void VectorAppend(std::vector<uint8_t>& vector, const void* data, size_t size) {
  vector.insert(vector.end(), static_cast<const uint8_t*>(data),
                static_cast<const uint8_t*>(data) + size);
}

template <typename T>
static void VectorAppend(std::vector<uint8_t>& vector, T value) {
  VectorAppend(vector, &value, sizeof(value));
}

static inline uint8_t EpIndexToAddress(uint8_t index) {
  return static_cast<uint8_t>((index & 0xF) | ((index & 0x10) << 3));
}

constexpr uint8_t kInEndpointStart = 17;
constexpr uint8_t kOutEndpointStart = 1;

class EndpointBuilder {
 public:
  explicit EndpointBuilder(uint8_t config_num, uint8_t endpoint_type, uint8_t endpoint_index,
                           bool in) {
    base_desc_.bm_attributes = endpoint_type;
    base_desc_.b_length = sizeof(base_desc_);
    base_desc_.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint);
    base_desc_.b_endpoint_address =
        EpIndexToAddress(endpoint_index + (in ? kInEndpointStart : kOutEndpointStart));
  }

  explicit EndpointBuilder(uint8_t config_num, fdescriptor::EndpointType endpoint_type,
                           uint8_t endpoint_index, fdescriptor::EndpointDirection direction)
      : EndpointBuilder(config_num, static_cast<uint8_t>(endpoint_type), endpoint_index,
                        direction == fdescriptor::EndpointDirection::kIn) {}

  void set_max_packet_size(uint16_t max_packet_size) {
    base_desc_.w_max_packet_size = max_packet_size;
  }

  void AddSsCompanion(const usb_ss_ep_comp_descriptor_t& comp) { VectorAppend(descriptors_, comp); }
  void AddSsCompanion(const void* desc, size_t desc_length) {
    VectorAppend(descriptors_, desc, desc_length);
  }
  void AddSsIsochCompanion(const usb_ss_isoch_ep_comp_descriptor_t& comp) {
    VectorAppend(descriptors_, comp);
  }
  void AddSsIsochCompanion(const void* desc, size_t desc_length) {
    VectorAppend(descriptors_, desc, desc_length);
  }

  std::vector<uint8_t> Generate() const {
    size_t total = sizeof(base_desc_) + descriptors_.size();
    std::vector<uint8_t> data;
    data.reserve(total);
    VectorAppend(data, base_desc_);
    VectorAppend(data, descriptors_.data(), descriptors_.size());
    return data;
  }

 private:
  std::vector<uint8_t> descriptors_;
  usb_endpoint_descriptor_t base_desc_ = {};
};

class InterfaceBuilder {
 public:
  explicit InterfaceBuilder(uint8_t config_num, uint8_t alt_setting = 0) {
    base_desc_.b_num_endpoints = 0;
    base_desc_.b_length = sizeof(base_desc_);
    base_desc_.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface);
    base_desc_.b_alternate_setting = alt_setting;
  }

  void AddEndpoint(const EndpointBuilder& builder) {
    auto data = builder.Generate();
    AddEndpoint(data.data(), data.size());
  }
  void AddEndpoint(void* desc, size_t desc_length) {
    VectorAppend(descriptors_, desc, desc_length);
    base_desc_.b_num_endpoints++;
  }

  std::vector<uint8_t> Generate() const {
    size_t total = sizeof(base_desc_) + descriptors_.size();
    std::vector<uint8_t> data;
    data.reserve(total);
    VectorAppend(data, base_desc_);
    VectorAppend(data, descriptors_.data(), descriptors_.size());
    return data;
  }

 private:
  std::vector<uint8_t> descriptors_;
  usb_interface_descriptor_t base_desc_ = {};
};

class ConfigurationBuilder {
 public:
  explicit ConfigurationBuilder(uint8_t config_num) {
    base_desc_.b_num_interfaces = 0;
    base_desc_.i_configuration = config_num;
    base_desc_.b_length = sizeof(base_desc_);
    // Total length is calculated in Generate()
    base_desc_.w_total_length = 0;
    base_desc_.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration);
  }

  void AddInterface(const InterfaceBuilder& builder) {
    auto data = builder.Generate();
    AddInterface(data.data(), data.size());
  }
  void AddInterface(void* interface_desc, size_t interface_desc_length) {
    VectorAppend(descriptors_, interface_desc, interface_desc_length);
    usb_interface_descriptor_t* intf =
        reinterpret_cast<usb_interface_descriptor_t*>(interface_desc);
    if (intf->b_alternate_setting == 0) {
      base_desc_.b_num_interfaces++;
    }

    size_t total = sizeof(base_desc_) + descriptors_.size();
    base_desc_.w_total_length = htole16(static_cast<uint16_t>(total));
  }

  std::vector<uint8_t> Generate() const {
    size_t total = sizeof(base_desc_) + descriptors_.size();
    std::vector<uint8_t> data;
    data.reserve(total);
    VectorAppend(data, base_desc_);
    VectorAppend(data, descriptors_.data(), descriptors_.size());
    return data;
  }

 private:
  std::vector<uint8_t> descriptors_;
  usb_configuration_descriptor_t base_desc_ = {};
};

class DeviceDescriptorBuilder {
 public:
  explicit DeviceDescriptorBuilder() {
    base_desc_.b_num_configurations = 0;
    base_desc_.b_length = sizeof(base_desc_);
    base_desc_.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice);
  }

  void set_vendor_id(uint16_t vendor_id) { base_desc_.id_vendor = vendor_id; }

  void set_product_id(uint16_t product_id) { base_desc_.id_product = product_id; }

  void AddConfiguration(const ConfigurationBuilder& builder) {
    auto data = builder.Generate();
    AddConfiguration(data.data(), data.size());
  }
  void AddConfiguration(void* config_desc, size_t config_desc_length) {
    VectorAppend(descriptors_, config_desc, config_desc_length);
    base_desc_.b_num_configurations++;
  }

  std::vector<uint8_t> Generate() {
    size_t total = sizeof(base_desc_) + descriptors_.size();
    std::vector<uint8_t> data;
    data.reserve(total);
    VectorAppend(data, base_desc_);
    VectorAppend(data, descriptors_.data(), descriptors_.size());
    return data;
  }

 private:
  std::vector<uint8_t> descriptors_;
  usb_device_descriptor_t base_desc_ = {};
};
}  // namespace usb
#endif  // SRC_DEVICES_USB_TESTING_DESCRIPTOR_BUILDER_DESCRIPTOR_BUILDER_H_
