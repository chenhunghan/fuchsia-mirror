// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_AUDIO_H_
#define SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_AUDIO_H_

// clang-format off

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <usb/descriptors.h>

// Audio and MIDI interface subclasses, class-specific descriptor types/subtypes, request codes,
// control selectors, terminal types, and format tags are defined in fuchsia.hardware.usb.descriptor.

// Top level header structure shared by all USB audio descriptors.
//
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // DescriptorType::kCsInterface
    uint8_t bDescriptorSubtype;
} __PACKED usb_audio_desc_header;

// Audio Control Interface descriptor definitions
//
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_HEADER
    uint16_t bcdADC;
    uint16_t wTotalLength;
    uint8_t bInCollection;
    uint8_t baInterfaceNr[];
} __PACKED usb_audio_ac_header_desc;

// Common header structure shared by all unit and terminal descriptors found in
// an Audio Control interface descriptor.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_.*_(TERMINAL|UNIT)
    uint8_t bID;
} __PACKED usb_audio_ac_ut_desc;

// Common header structure shared by all terminal descriptors found in an Audio
// Control interface descriptor.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_(INPUT|OUTPUT)_TERMINAL
    uint8_t bTerminalID;
    uint16_t wTerminalType;
    uint8_t bAssocTerminal;
} __PACKED usb_audio_ac_terminal_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_INPUT_TERMINAL
    uint8_t bTerminalID;
    uint16_t wTerminalType;
    uint8_t bAssocTerminal;
    uint8_t bNrChannels;
    uint16_t wChannelConfig;
    uint8_t iChannelNames;
    uint8_t iTerminal;
} __PACKED usb_audio_ac_input_terminal_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_OUTPUT_TERMINAL
    uint8_t bTerminalID;
    uint16_t wTerminalType;
    uint8_t bAssocTerminal;
    uint8_t bSourceID;
    uint8_t iTerminal;
} __PACKED usb_audio_ac_output_terminal_desc;

// Note: Mixer unit descriptors contain two inlined variable length arrays, each
// with descriptor data following them.  They are therefor described using 3
// structure definitions which are logically concatenated, but separated by the
// inline arrays.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_MIXER_UNIT
    uint8_t bUnitID;
    uint8_t bNrInPins;
    uint8_t baSourceID[];
} __PACKED usb_audio_ac_mixer_unit_desc_0;

typedef struct {
    uint8_t bNrChannels;
    uint16_t wChannelConfig;
    uint8_t iChannelNames;
    uint8_t bmControls[];
} __PACKED usb_audio_ac_mixer_unit_desc_1;

typedef struct {
    uint8_t iMixer;
} __PACKED usb_audio_ac_mixer_unit_desc_2;

// Note: Selector unit descriptors contain an inlined variable length array with
// descriptor data following it.  They are therefor described using 2 structure
// definitions which are logically concatenated, but separated by the inline
// array.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_SELECTOR_UNIT
    uint8_t bUnitID;
    uint8_t bNrInPins;
    uint8_t baSourceID[];
} __PACKED usb_audio_ac_selector_unit_desc_0;

typedef struct {
    uint8_t iSelector;
} __PACKED usb_audio_ac_selector_unit_desc_1;

// Note: Feature unit descriptors contain an inlined variable length array with
// descriptor data following it.  They are therefor described using 2 structure
// definitions which are logically concatenated, but separated by the inline
// array.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_FEATURE_UNIT
    uint8_t bUnitID;
    uint8_t bSourceID;
    uint8_t bControlSize;
    uint8_t bmaControls[];
} __PACKED usb_audio_ac_feature_unit_desc_0;

typedef struct {
    uint8_t iFeature;
} __PACKED usb_audio_ac_feature_unit_desc_1;

// Note: Processing unit descriptors contain two inlined variable length arrays,
// each with descriptor data following them.  They are therefor described using
// 3 structure definitions which are logically concatenated, but separated by
// the inline arrays.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_PROCESSING_UNIT
    uint8_t bUnitID;
    uint16_t wProcessType;
    uint8_t bNrInPins;
    uint8_t baSourceID[];
} __PACKED usb_audio_ac_processing_unit_desc_0;

typedef struct {
    uint8_t bNrChannels;
    uint16_t wChannelConfig;
    uint8_t iChannelNames;
    uint8_t bControlSize;
    uint8_t bmControls[];
} __PACKED usb_audio_ac_processing_unit_desc_1;

typedef struct {
    uint8_t iProcessing;
    // Note: The Process-specific control structure follows this with the
    // structure type determined by wProcessType
    // TODO(johngro) : Define the process specific control structures.  As of
    // the 1.0 revision of the USB audio spec, the types to be defined are...
    //
    // ** Up/Down-mix
    // ** Dolby Prologic
    // ** 3D-Stereo Extender
    // ** Reverberation
    // ** Chorus
    // ** Dynamic Range Compressor
} __PACKED usb_audio_ac_processing_unit_desc_2;

// Note: Extension unit descriptors contain two inlined variable length arrays,
// each with descriptor data following them.  They are therefor described using
// 3 structure definitions which are logically concatenated, but separated by
// the inline arrays.
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AC_EXTENSION_UNIT
    uint8_t bUnitID;
    uint16_t wExtensionCode;
    uint8_t bNrInPins;
    uint8_t baSourceID[];
} __PACKED usb_audio_ac_extension_unit_desc_0;

typedef struct {
    uint8_t bNrChannels;
    uint16_t wChannelConfig;
    uint8_t iChannelNames;
    uint8_t bControlSize;
    uint8_t bmControls[];
} __PACKED usb_audio_ac_extension_unit_desc_1;

typedef struct {
    uint8_t iExtension;
} __PACKED usb_audio_ac_extension_unit_desc_2;

// Audio Streaming Interface descriptor definitions
//
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AS_GENERAL
    uint8_t bTerminalLink;
    uint8_t bDelay;
    uint16_t wFormatTag;
} __PACKED usb_audio_as_header_desc;

typedef struct {
    uint8_t freq[3];            // 24 bit unsigned integer, little-endian
} __PACKED usb_audio_as_samp_freq;

// Common header used by all format type descriptors
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AS_FORMAT_TYPE
    uint8_t bFormatType;
} __PACKED usb_audio_as_format_type_hdr;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_AUDIO_AS_FORMAT_TYPE
    uint8_t bFormatType;            // USB_AUDIO_FORMAT_TYPE_I
    uint8_t bNrChannels;
    uint8_t bSubFrameSize;
    uint8_t bBitResolution;
    uint8_t bSamFreqType;           // number of sampling frequencies
    usb_audio_as_samp_freq tSamFreq[]; // list of sampling frequencies (3 bytes each)
} __PACKED usb_audio_as_format_type_i_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_ENDPOINT
    uint8_t bDescriptorSubtype;     // USB_AUDIO_EP_GENERAL
    uint8_t bmAttributes;
    uint8_t bLockDelayUnits;
    uint16_t wLockDelay;
} __PACKED usb_audio_as_isoch_ep_desc;

// MIDI Streaming Interface descriptor definitions
//
typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_MIDI_MS_HEADER
    uint16_t bcdMSC;
    uint16_t wTotalLength;
} __PACKED usb_midi_ms_header_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_MIDI_IN_JACK
    uint8_t bJackType;
    uint8_t bJackID;
    uint8_t iJack;
} __PACKED usb_midi_ms_in_jack_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_INTERFACE
    uint8_t bDescriptorSubtype;     // USB_MIDI_OUT_JACK
    uint8_t bJackType;
    uint8_t bJackID;
    uint8_t bNrInputPins;
    uint8_t baSourceID;
    uint8_t baSourcePin;
} __PACKED usb_midi_ms_out_jack_desc;

typedef struct {
    uint8_t bLength;
    uint8_t bDescriptorType;        // USB_AUDIO_CS_ENDPOINT
    uint8_t bDescriptorSubtype;     // USB_MIDI_MS_GENERAL
    uint8_t bNumEmbMIDIJack;
    uint8_t baAssocJackID[];
} __PACKED usb_midi_ms_endpoint_desc;

static_assert(sizeof(usb_audio_desc_header) == 3);
static_assert(sizeof(usb_audio_ac_header_desc) == 8);
static_assert(sizeof(usb_audio_ac_ut_desc) == 4);
static_assert(sizeof(usb_audio_ac_terminal_desc) == 7);
static_assert(sizeof(usb_audio_ac_input_terminal_desc) == 12);
static_assert(sizeof(usb_audio_ac_output_terminal_desc) == 9);
static_assert(sizeof(usb_audio_ac_mixer_unit_desc_0) == 5);
static_assert(sizeof(usb_audio_ac_mixer_unit_desc_1) == 4);
static_assert(sizeof(usb_audio_ac_mixer_unit_desc_2) == 1);
static_assert(sizeof(usb_audio_ac_selector_unit_desc_0) == 5);
static_assert(sizeof(usb_audio_ac_selector_unit_desc_1) == 1);
static_assert(sizeof(usb_audio_ac_feature_unit_desc_0) == 6);
static_assert(sizeof(usb_audio_ac_feature_unit_desc_1) == 1);
static_assert(sizeof(usb_audio_ac_processing_unit_desc_0) == 7);
static_assert(sizeof(usb_audio_ac_processing_unit_desc_1) == 5);
static_assert(sizeof(usb_audio_ac_processing_unit_desc_2) == 1);
static_assert(sizeof(usb_audio_ac_extension_unit_desc_0) == 7);
static_assert(sizeof(usb_audio_ac_extension_unit_desc_1) == 5);
static_assert(sizeof(usb_audio_ac_extension_unit_desc_2) == 1);
static_assert(sizeof(usb_audio_as_header_desc) == 7);
static_assert(sizeof(usb_audio_as_samp_freq) == 3);
static_assert(sizeof(usb_audio_as_format_type_hdr) == 4);
static_assert(sizeof(usb_audio_as_format_type_i_desc) == 8);
static_assert(sizeof(usb_audio_as_isoch_ep_desc) == 7);
static_assert(sizeof(usb_midi_ms_header_desc) == 7);
static_assert(sizeof(usb_midi_ms_in_jack_desc) == 6);
static_assert(sizeof(usb_midi_ms_out_jack_desc) == 8);
static_assert(sizeof(usb_midi_ms_endpoint_desc) == 4);

#endif  // SRC_DEVICES_USB_LIB_USB_INCLUDE_USB_AUDIO_H_
