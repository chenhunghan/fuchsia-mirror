// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_CDC_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_CDC_H_

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <usb/descriptors.h>

// CDC subclasses, descriptor subtypes, requests, notifications, and packet filter bits are
// imported via <usb/descriptors.h>

typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;  // CdcDescriptorSubtype::kHeader
  uint16_t bcdCDC;
} __attribute__((packed)) usb_cs_header_interface_descriptor_t;

typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;  // CdcDescriptorSubtype::kCallMgmt
  uint8_t bmCapabilities;
  uint8_t bDataInterface;
} __attribute__((packed)) usb_cs_call_mgmt_interface_descriptor_t;

typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;  // CdcDescriptorSubtype::kAbstractCtrlMgmt
  uint8_t bmCapabilities;
} __attribute__((packed)) usb_cs_abstract_ctrl_mgmt_interface_descriptor_t;

typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;  // CdcDescriptorSubtype::kUnion
  uint8_t bControlInterface;
  uint8_t bSubordinateInterface[];
} __attribute__((packed)) usb_cs_union_interface_descriptor_t;

// fixed size version of usb_cs_union_interface_descriptor_t
typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;  // CdcDescriptorSubtype::kUnion
  uint8_t bControlInterface;
  uint8_t bSubordinateInterface;
} __attribute__((packed)) usb_cs_union_interface_descriptor_1_t;

typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;  // CdcDescriptorSubtype::kEthernet
  uint8_t iMACAddress;
  uint32_t bmEthernetStatistics;
  uint16_t wMaxSegmentSize;
  uint16_t wNumberMCFilters;
  uint8_t bNumberPowerFilters;
} __attribute__((packed)) usb_cs_ethernet_interface_descriptor_t;

typedef struct {
  uint8_t bmRequestType;
  uint8_t bNotification;
  uint16_t wValue;
  uint16_t wIndex;
  uint16_t wLength;
} __attribute__((packed)) usb_cdc_notification_t;

typedef struct {
  usb_cdc_notification_t notification;
  uint32_t downlink_br;
  uint32_t uplink_br;
} __attribute__((packed)) usb_cdc_speed_change_notification_t;

static_assert(sizeof(usb_cs_header_interface_descriptor_t) == 5);
static_assert(sizeof(usb_cs_call_mgmt_interface_descriptor_t) == 5);
static_assert(sizeof(usb_cs_abstract_ctrl_mgmt_interface_descriptor_t) == 4);
static_assert(sizeof(usb_cs_union_interface_descriptor_t) == 4);
static_assert(sizeof(usb_cs_union_interface_descriptor_1_t) == 5);
static_assert(sizeof(usb_cs_ethernet_interface_descriptor_t) == 13);
static_assert(sizeof(usb_cdc_notification_t) == 8);
static_assert(sizeof(usb_cdc_speed_change_notification_t) == 16);

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_CDC_H_
