// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_VIDEO_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_VIDEO_H_

// clang-format off

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <usb/descriptors.h>

// Video interface subclasses, class-specific descriptor types/subtypes, request codes,
// control selectors, and payload header flags are defined in fuchsia.hardware.usb.descriptor.

// TODO(https://fxbug.dev/42061398): Change fields to use snake_case naming convention.

// header for usb_video_vc_* below
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsInterface
    uint8_t bDescriptorSubtype;
} __PACKED usb_video_vc_desc_header;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsInterface
    uint8_t bDescriptorSubtype;     // VideoVcDescriptorSubtype::kHeader
    uint16_t bcdUVC;
    uint16_t wTotalLength;
    uint32_t dwClockFrequency;
    uint8_t bInCollection;
    uint8_t baInterfaceNr[];
} __PACKED usb_video_vc_header_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsInterface
    uint8_t bDescriptorSubtype;     // VideoVcDescriptorSubtype::kInputTerminal
    uint8_t bTerminalID;
    uint16_t wTerminalType;
    uint8_t bAssocTerminal;
    uint8_t iTerminal;
} __PACKED usb_video_vc_input_terminal_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsInterface
    uint8_t bDescriptorSubtype;     // VideoVcDescriptorSubtype::kOutputTerminal
    uint8_t bTerminalID;
    uint16_t wTerminalType;
    uint8_t bAssocTerminal;
    uint8_t bSourceID;
    uint8_t iTerminal;
} __PACKED usb_video_vc_output_terminal_desc;

// class specific VC interrupt endpoint descriptor
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsEndpoint
    uint8_t bDescriptorSubtype;     // EndpointType::kInterrupt
    uint16_t wMaxTransferSize;
} __PACKED usb_video_vc_interrupt_endpoint_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsInterface
    uint8_t bDescriptorSubtype;     // VideoVsDescriptorSubtype::kInputHeader
    uint8_t bNumFormats;
    uint16_t wTotalLength;
    uint8_t bEndpointAddress;
    uint8_t bmInfo;
    uint8_t bTerminalLink;
    uint8_t bStillCaptureMethod;
    uint8_t bTriggerSupport;
    uint8_t bTriggerUsage;
    uint8_t bControlSize;
    uint8_t bmaControls[];
} __PACKED usb_video_vs_input_header_desc;

// Definition of above without the variable component:
typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;     // DescriptorType::kCsInterface
  uint8_t bDescriptorSubtype;  // VideoVsDescriptorSubtype::kInputHeader
  uint8_t bNumFormats;
  uint16_t wTotalLength;
  uint8_t bEndpointAddress;
  uint8_t bmInfo;
  uint8_t bTerminalLink;
  uint8_t bStillCaptureMethod;
  uint8_t bTriggerSupport;
  uint8_t bTriggerUsage;
  uint8_t bControlSize;
} __PACKED usb_video_vs_input_header_desc_short;

// A GUID consists of a:
//  - four-byte integer
//  - two-byte integer
//  - two-byte integer
//  - eight-byte array
//
// The string representation uses big endian format, so to convert it
// to a byte array we need to reverse the byte order of the three integers.
//
// See USB Video Class revision 1.5, FAQ section 2.9
// for GUID Data Structure Layout.

inline constexpr char kUsbVideoGuidYuy2String[] = "32595559-0000-0010-8000-00AA00389B71";
inline constexpr char kUsbVideoGuidNv12String[] = "3231564E-0000-0010-8000-00AA00389B71";
inline constexpr char kUsbVideoGuidM420String[] = "3032344D-0000-0010-8000-00AA00389B71";
inline constexpr char kUsbVideoGuidI420String[] = "30323449-0000-0010-8000-00AA00389B71";

inline constexpr uint8_t kUsbVideoGuidYuy2Value[fuchsia_hardware_usb_descriptor::kVideoGuidLength] = {
    0x59, 0x55, 0x59, 0x32,
    0x00, 0x00,
    0x10, 0x00,
    0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71
};

inline constexpr uint8_t kUsbVideoGuidNv12Value[fuchsia_hardware_usb_descriptor::kVideoGuidLength] = {
    0x4e, 0x56, 0x31, 0x32,
    0x00, 0x00,
    0x10, 0x00,
    0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71
};

inline constexpr uint8_t kUsbVideoGuidM420Value[fuchsia_hardware_usb_descriptor::kVideoGuidLength] = {
    0x4d, 0x34, 0x32, 0x30,
    0x00, 0x00,
    0x10, 0x00,
    0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71
};

inline constexpr uint8_t kUsbVideoGuidI420Value[fuchsia_hardware_usb_descriptor::kVideoGuidLength] = {
    0x49, 0x34, 0x32, 0x30,
    0x00, 0x00,
    0x10, 0x00,
    0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71
};

// Header common to all frame descriptors:
typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;  // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;
  uint8_t bFormatIndex;
  uint8_t bNumFrameDescriptors;
} __PACKED usb_video_format_header;

// USB Video Payload Uncompressed
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;         // DescriptorType::kCsInterface
    uint8_t bDescriptorSubType;      // VideoVsDescriptorSubtype::kFormatUncompressed
    uint8_t bFormatIndex;
    uint8_t bNumFrameDescriptors;
    uint8_t guidFormat[fuchsia_hardware_usb_descriptor::kVideoGuidLength];
    uint8_t bBitsPerPixel;
    uint8_t bDefaultFrameIndex;
    uint8_t bAspectRatioX;
    uint8_t bAspectRatioY;
    uint8_t bmInterfaceFlags;
    uint8_t bCopyProtect;
} __PACKED usb_video_vs_uncompressed_format_desc;

// USB Video Payload MJPEG
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;         // DescriptorType::kCsInterface
    uint8_t bDescriptorSubType;      // VideoVsDescriptorSubtype::kFormatMjpeg
    uint8_t bFormatIndex;
    uint8_t bNumFrameDescriptors;
    uint8_t bmFlags;
    uint8_t bDefaultFrameIndex;
    uint8_t bAspectRatioX;
    uint8_t bAspectRatioY;
    uint8_t bmInterfaceFlags;
    uint8_t bCopyProtect;
} __PACKED usb_video_vs_mjpeg_format_desc;

// USB Video Payload Frame Based
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;         // DescriptorType::kCsInterface
    uint8_t bDescriptorSubType;      // VideoVsDescriptorSubtype::kFormatFrameBased
    uint8_t bFormatIndex;
    uint8_t bNumFrameDescriptors;
    uint8_t guidFormat[fuchsia_hardware_usb_descriptor::kVideoGuidLength];
    uint8_t bBitsPerPixel;
    uint8_t bDefaultFrameIndex;
    uint8_t bAspectRatioX;
    uint8_t bAspectRatioY;
    uint8_t bmInterfaceFlags;
    uint8_t bCopyProtect;
    uint8_t bVariableSize;
} __PACKED usb_video_vs_frame_based_format_desc;

// Header common to all frame descriptors
typedef struct {
  uint8_t bLength;
  uint8_t bDescriptorType;  // DescriptorType::kCsInterface
  uint8_t bDescriptorSubType;
  uint8_t bFrameIndex;
} __PACKED usb_video_frame_header;

// Uncompressed and MJPEG formats have the same frame descriptor structure.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;         // DescriptorType::kCsInterface
    uint8_t bDescriptorSubType;      // VideoVsDescriptorSubtype::kFrameUncompressed / kFrameMjpeg
    uint8_t bFrameIndex;
    uint8_t bmCapabilities;
    uint16_t wWidth;
    uint16_t wHeight;
    uint32_t dwMinBitRate;
    uint32_t dwMaxBitRate;
    uint32_t dwMaxVideoFrameBufferSize;
    uint32_t dwDefaultFrameInterval;
    uint8_t bFrameIntervalType;
    uint32_t dwFrameInterval[];
} __PACKED usb_video_vs_frame_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;         // DescriptorType::kCsInterface
    uint8_t bDescriptorSubType;      // VideoVsDescriptorSubtype::kFrameFrameBased
    uint8_t bFrameIndex;
    uint8_t bmCapabilities;
    uint16_t wWidth;
    uint16_t wHeight;
    uint32_t dwMinBitRate;
    uint32_t dwMaxBitRate;
    uint32_t dwDefaultFrameInterval;
    uint8_t bFrameIntervalType;
    uint32_t dwBytesPerLine;
    uint32_t dwFrameInterval[];
} __PACKED usb_video_vs_frame_based_frame_desc;

typedef struct {
   uint16_t bmHint;
   uint8_t bFormatIndex;
   uint8_t bFrameIndex;
   uint32_t dwFrameInterval;
   uint16_t wKeyFrameRate;
   uint16_t wPFrameRate;
   uint16_t wCompQuality;
   uint16_t wCompWindowSize;
   uint16_t wDelay;
   uint32_t dwMaxVideoFrameSize;
   uint32_t dwMaxPayloadTransferSize;
   // The following fields are optional.
   uint32_t dwClockFrequency;
   uint8_t bmFramingInfo;
   uint8_t bPreferedVersion;
   uint8_t bMinVersion;
   uint8_t bMaxVersion;
   uint8_t bUsage;
   uint8_t bBitDepthLuma;
   uint8_t bmSettings;
   uint8_t bMaxNumberOfRefFramesPlus1;
   uint16_t bmRateControlModes;
   uint32_t bmLayoutPerStream;
} __PACKED usb_video_vc_probe_and_commit_controls;

// Common header for all payloads.
typedef struct {
    uint8_t bHeaderLength;
    uint8_t bmHeaderInfo;

} __PACKED usb_video_vs_payload_header;

typedef struct {
    uint8_t bHeaderLength;
    uint8_t bmHeaderInfo;
    uint32_t dwPresentationTime;
    uint32_t scrSourceTimeClock;
    // Frame number when the source clock was sampled.
    uint16_t scrSourceClockSOFCounter;
} __PACKED usb_video_vs_uncompressed_payload_header;

static_assert(sizeof(usb_video_vc_desc_header) == 3);
static_assert(sizeof(usb_video_vc_header_desc) == 12);
static_assert(sizeof(usb_video_vc_input_terminal_desc) == 8);
static_assert(sizeof(usb_video_vc_output_terminal_desc) == 9);
static_assert(sizeof(usb_video_vc_interrupt_endpoint_desc) == 5);
static_assert(sizeof(usb_video_vs_input_header_desc) == 13);
static_assert(sizeof(usb_video_vs_input_header_desc_short) == 13);
static_assert(sizeof(usb_video_format_header) == 5);
static_assert(sizeof(usb_video_vs_uncompressed_format_desc) == 27);
static_assert(sizeof(usb_video_vs_mjpeg_format_desc) == 11);
static_assert(sizeof(usb_video_vs_frame_based_format_desc) == 28);
static_assert(sizeof(usb_video_frame_header) == 4);
static_assert(sizeof(usb_video_vs_frame_desc) == 26);
static_assert(sizeof(usb_video_vs_frame_based_frame_desc) == 26);
static_assert(sizeof(usb_video_vc_probe_and_commit_controls) == 44);
static_assert(sizeof(usb_video_vs_payload_header) == 2);
static_assert(sizeof(usb_video_vs_uncompressed_payload_header) == 12);

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_VIDEO_H_
