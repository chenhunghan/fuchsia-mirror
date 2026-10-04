// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-cdc-ecm-lib.h"

#include <lib/fit/defer.h>
#include <zircon/errors.h>

#include "fuchsia/hardware/usb/descriptor/c/banjo.h"
#include "usb/usb.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_cdc_ecm {

zx::result<MacAddress> UsbCdcDescriptorParser::ParseMacAddress(
    usb::UsbDevice& usb, const usb_cs_ethernet_interface_descriptor_t* desc) {
  if (desc->iMACAddress == 0) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  // Read string descriptor for MAC address (string index is in iMACAddress field)
  size_t out_length;
  uint8_t str_desc_buf[kExpectedStringSize];
  zx_status_t status = usb.GetDescriptor(
      0, fidl::ToUnderlying(fdescriptor::DescriptorType::kString), desc->iMACAddress, str_desc_buf,
      sizeof(str_desc_buf), ZX_TIME_INFINITE, &out_length);
  if (status != ZX_OK) {
    fdf::error("Error reading MAC address");
    return zx::error(status);
  }
  if (out_length != kExpectedStringSize) {
    fdf::error("MAC address string incorrect length (saw {}, expected {})", out_length,
               kExpectedStringSize);
    return zx::error(ZX_ERR_IO);
  }

  // Convert MAC address to something more machine-friendly
  auto str_desc = reinterpret_cast<usb_string_descriptor_t*>(str_desc_buf);
  uint8_t* str = str_desc->b_string;
  size_t ndx;
  MacAddress mac_addr;
  for (ndx = 0; ndx < ETH_MAC_SIZE * 4; ndx++) {
    if (ndx % 2 == 1) {
      if (str[ndx] != 0) {
        fdf::error("MAC address contains invalid characters");
        return zx::error(ZX_ERR_IO);
      }
      continue;
    }
    uint8_t value;
    if (str[ndx] >= '0' && str[ndx] <= '9') {
      value = str[ndx] - '0';
    } else if (str[ndx] >= 'A' && str[ndx] <= 'F') {
      value = (str[ndx] - 'A') + 0xa;
    } else {
      fdf::error("MAC address contains invalid characters");
      return zx::error(ZX_ERR_IO);
    }
    if (ndx % 4 == 0) {
      mac_addr[ndx / 4] = (uint8_t)(value << 4);
    } else {
      mac_addr[ndx / 4] |= value;
    }
  }

  fdf::info("MAC address is {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", mac_addr[0], mac_addr[1],
            mac_addr[2], mac_addr[3], mac_addr[4], mac_addr[5]);
  return zx::ok(mac_addr);
}

zx::result<UsbCdcDescriptorParser> UsbCdcDescriptorParser::Parse(usb::UsbDevice& usb) {
  std::optional<usb::InterfaceList> interfaces;
  zx_status_t status = usb::InterfaceList::Create(usb, false, &interfaces);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  std::optional<EcmEndpoint> int_ep;
  std::optional<EcmEndpoint> tx_ep;
  std::optional<EcmEndpoint> rx_ep;
  std::optional<EcmInterface> default_ifc;
  std::optional<EcmInterface> data_ifc;
  std::optional<EcmInterface> comm_ifc;

  // Find default interface.
  for (const usb::Interface& interface : *interfaces) {
    const usb_interface_descriptor_t* desc = interface.descriptor();
    if (desc->b_interface_class != fidl::ToUnderlying(fdescriptor::UsbClass::kCdc)) {
      continue;
    }
    if (desc->b_num_endpoints != 0) {
      continue;
    }
    if (default_ifc.has_value()) {
      fdf::error("Multiple default interfaces found");
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    default_ifc = EcmInterface(desc);
  }

  // Find data interface.
  for (const usb::Interface& interface : *interfaces) {
    const usb_interface_descriptor_t* desc = interface.descriptor();
    if (desc->b_interface_class != fidl::ToUnderlying(fdescriptor::UsbClass::kCdc)) {
      continue;
    }
    if (desc->b_num_endpoints != 2) {
      continue;
    }
    if (data_ifc.has_value()) {
      fdf::error("Multiple data interfaces found");
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    data_ifc = EcmInterface(desc);

    for (const auto& endpoint : interface.GetEndpointList()) {
      const usb_endpoint_descriptor_t* endpoint_desc = endpoint.descriptor();
      if (usb_ep_direction(endpoint_desc) == fdescriptor::EndpointDirection::kOut &&
          usb_ep_type(endpoint_desc) == fdescriptor::EndpointType::kBulk) {
        if (tx_ep.has_value()) {
          fdf::error("Multiple tx endpoint descriptors");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
        }
        tx_ep = EcmEndpoint(endpoint_desc);
      } else if (usb_ep_direction(endpoint_desc) == fdescriptor::EndpointDirection::kIn &&
                 usb_ep_type(endpoint_desc) == fdescriptor::EndpointType::kBulk) {
        if (rx_ep.has_value()) {
          fdf::error("Multiple rx endpoint descriptors");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
        }
        rx_ep = EcmEndpoint(endpoint_desc);
      } else {
        fdf::error("Unrecognized endpoint");
        return zx::error(ZX_ERR_NOT_SUPPORTED);
      }
    }
  }

  // Find communications interface, which has CDC headers and interrupt endpoint.
  const usb_cs_header_interface_descriptor_t* cdc_header_desc = nullptr;
  const usb_cs_ethernet_interface_descriptor_t* cdc_eth_desc = nullptr;
  for (const usb::Interface& interface : *interfaces) {
    if (interface.descriptor()->b_interface_class !=
        fidl::ToUnderlying(fdescriptor::UsbClass::kComm)) {
      continue;
    }
    if (comm_ifc.has_value()) {
      fdf::error("Multiple communications interfaces found");
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    comm_ifc = EcmInterface(interface.descriptor());

    for (auto& descriptor : interface.GetDescriptorList()) {
      if (descriptor.b_descriptor_type !=
          fidl::ToUnderlying(fdescriptor::DescriptorType::kCsInterface)) {
        continue;
      }
      if (descriptor.b_length < sizeof(usb_cs_interface_descriptor_t)) {
        fdf::warn("Malformed class specific descriptor");
        continue;
      }

      const usb_cs_interface_descriptor_t* cs_ifc_desc =
          reinterpret_cast<const usb_cs_interface_descriptor_t*>(&descriptor);

      if (cs_ifc_desc->b_descriptor_sub_type ==
          fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kHeader)) {
        if (cdc_header_desc != nullptr) {
          fdf::error("Multiple CDC headers");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
        }
        if (descriptor.b_length < sizeof(usb_cs_header_interface_descriptor_t)) {
          fdf::warn("Malformed CDC header descriptor");
          continue;
        }
        cdc_header_desc =
            reinterpret_cast<const usb_cs_header_interface_descriptor_t*>(&descriptor);
      } else if (cs_ifc_desc->b_descriptor_sub_type ==
                 fidl::ToUnderlying(fdescriptor::CdcDescriptorSubtype::kEthernet)) {
        if (cdc_eth_desc != nullptr) {
          fdf::error("Multiple CDC ethernet descriptors");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
        }
        if (descriptor.b_length < sizeof(usb_cs_ethernet_interface_descriptor_t)) {
          fdf::warn("Malformed CDC ethernet descriptor");
          continue;
        }
        cdc_eth_desc = reinterpret_cast<const usb_cs_ethernet_interface_descriptor_t*>(&descriptor);
      }
    }

    for (const auto& endpoint : interface.GetEndpointList()) {
      const usb_endpoint_descriptor_t* endpoint_desc = endpoint.descriptor();
      if (usb_ep_direction(endpoint_desc) == fdescriptor::EndpointDirection::kIn &&
          usb_ep_type(endpoint_desc) == fdescriptor::EndpointType::kInterrupt) {
        if (int_ep.has_value()) {
          fdf::error("Multiple interrupt endpoint descriptors");
          return zx::error(ZX_ERR_NOT_SUPPORTED);
        }
        int_ep = EcmEndpoint(endpoint_desc);
      }
    }
  }

  if (cdc_header_desc == nullptr || cdc_eth_desc == nullptr) {
    fdf::error("CDC {} descriptor(s) not found", cdc_header_desc ? "ethernet"
                                                 : cdc_eth_desc  ? "header"
                                                                 : "ethernet and header");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  if (!int_ep.has_value() || !tx_ep.has_value() || !rx_ep.has_value()) {
    fdf::error("Missing one or more required endpoints");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  if (!default_ifc.has_value()) {
    fdf::error("Unable to find CDC default interface");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  if (!data_ifc.has_value()) {
    fdf::error("Unable to find CDC data interface");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  if (!comm_ifc.has_value()) {
    fdf::error("Unable to find CDC comm interface");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  // Parse the information in the CDC descriptors. The temporary is used because the bcdCDC field
  // resides in a packed struct and is not 2-byte aligned. The alignment trips up ubsan when passing
  // as a reference to fdf::debug. Creating a stack variable ensures proper alignment. This is only
  // necessary due to the use of fdf::debug.
  uint16_t bcd_cdc = cdc_header_desc->bcdCDC;
  fdf::debug("Device reports CDC version as 0x{:x}", bcd_cdc);
  if (cdc_header_desc->bcdCDC < kCdcSupportedVersion) {
    fdf::error("Unable to parse cdc header");
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }

  const uint16_t mtu = cdc_eth_desc->wMaxSegmentSize;

  auto mac_addr = UsbCdcDescriptorParser::ParseMacAddress(usb, cdc_eth_desc);
  if (mac_addr.is_error()) {
    fdf::error("Unable to parse cdc ethernet descriptor");
    return mac_addr.take_error();
  }

  return zx::ok(UsbCdcDescriptorParser(int_ep.value(), tx_ep.value(), rx_ep.value(),
                                       default_ifc.value(), data_ifc.value(), comm_ifc.value(), mtu,
                                       mac_addr.value()));
}

}  // namespace usb_cdc_ecm
