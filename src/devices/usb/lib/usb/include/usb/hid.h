// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_HID_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_HID_H_

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <usb/descriptors.h>

// HID request values, protocols, and subclasses are imported as FIDL enum classes via
// <usb/descriptors.h>

typedef struct {
  uint8_t bDescriptorType;
  uint16_t wDescriptorLength;
} __attribute__((packed)) usb_hid_descriptor_entry_t;

typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;
  uint16_t bcdHID;
  uint8_t bCountryCode;
  uint8_t bNumDescriptors;
  usb_hid_descriptor_entry_t descriptors[];
} __attribute__((packed)) usb_hid_descriptor_t;

static_assert(sizeof(usb_hid_descriptor_entry_t) == 3);
static_assert(sizeof(usb_hid_descriptor_t) == 6);

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_HID_H_
