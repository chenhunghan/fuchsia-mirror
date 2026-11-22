// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_LIB_NETBOOT_INCLUDE_LIB_NETBOOT_NETBOOT_H_
#define SRC_BRINGUP_LIB_NETBOOT_INCLUDE_LIB_NETBOOT_NETBOOT_H_

#include <limits.h>
#include <stddef.h>
#include <stdint.h>

// clang-format off

#define NETBOOT_BOOTLOADER_VERSION "0.7.22"

#define NETBOOT_MAGIC              0xAA774217
#define NETBOOT_DEBUGLOG_MAGIC     0xAEAE1123

#define NETBOOT_PORT_SERVER        33330
#define NETBOOT_PORT_ADVERT        33331
#define NETBOOT_PORT_CMD_START     33332
#define NETBOOT_PORT_CMD_END       33339

#define NETBOOT_COMMAND_ACK               0   // arg=0 or -err, NETBOOT_READ: data=data
#define NETBOOT_COMMAND_EXECUTE           1   // arg=0, data=command
#define NETBOOT_COMMAND_QUERY             5   // arg=0, data=hostname (or "*")
#define NETBOOT_COMMAND_SHELL_CMD         6   // arg=0, data=command string
#define NETBOOT_COMMAND_REBOOT            12  // arg=0
#define NETBOOT_COMMAND_GET_ADVERT        13  // arg=0

#define NETBOOT_COMMAND_ADVERTISE          0x77777777

#define NETBOOT_COMMAND_ERROR_GENERIC      0x80000000
#define NETBOOT_COMMAND_ERROR_BAD_CMD      0x80000001
#define NETBOOT_COMMAND_ERROR_BAD_PARAM    0x80000002
#define NETBOOT_COMMAND_ERROR_TOO_LARGE    0x80000003
#define NETBOOT_COMMAND_ERROR_BAD_FILE     0x80000004

#define NETBOOT_VERSION_1_0  0x0001000
#define NETBOOT_VERSION_1_1  0x0001010
#define NETBOOT_VERSION_1_2  0x0001020
#define NETBOOT_VERSION_1_3  0x0001030
#define NETBOOT_VERSION_CURRENT NETBOOT_VERSION_1_3

#define NETBOOT_MAX_NODENAME_LEN 64

typedef struct {
    uint32_t magic;
    uint32_t cookie;
    uint32_t cmd;
    uint32_t arg;
} netboot_message_header_t;

#define NETBOOT_DEBUGLOG_PORT_SERVER  33337
#define NETBOOT_DEBUGLOG_PORT_ACK     33338

#define NETBOOT_DEBUGLOG_MAX_DATA 1216

typedef struct {
    uint32_t magic;
    uint32_t seqno;
    char nodename[NETBOOT_MAX_NODENAME_LEN];
    char data[NETBOOT_DEBUGLOG_MAX_DATA];
} netboot_debuglog_packet_t;

#endif  // SRC_BRINGUP_LIB_NETBOOT_INCLUDE_LIB_NETBOOT_NETBOOT_H_
