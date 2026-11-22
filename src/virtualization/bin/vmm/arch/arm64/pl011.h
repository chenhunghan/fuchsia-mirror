// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_VIRTUALIZATION_BIN_VMM_ARCH_ARM64_PL011_H_
#define SRC_VIRTUALIZATION_BIN_VMM_ARCH_ARM64_PL011_H_

#include <lib/stdcompat/span.h>
#include <lib/zx/socket.h>

#include <mutex>

#include "src/virtualization/bin/vmm/io.h"
#include "src/virtualization/bin/vmm/platform_device.h"

class Guest;

// PL011 registers.
enum class Pl011Register : uint64_t {
  DR = 0x00,
  FR = 0x18,
  IBRD = 0x24,
  FBRD = 0x28,
  LCR = 0x2c,
  CR = 0x30,
  IFLS = 0x34,
  IMSC = 0x38,
  ICR = 0x44,
};

constexpr uint16_t kPl011FlagRegisterRxFifoEmpty = 1u << 4u;
constexpr uint16_t kPl011FlagRegisterTxFifoEmpty = 1u << 7u;

// Implements the PL011 UART.
class Pl011 : public IoHandler, public PlatformDevice {
 public:
  Pl011(zx::socket socket);
  zx_status_t Init(Guest* guest);

  // |IoHandler|
  zx_status_t Read(uint64_t addr, IoValue* value) override;
  zx_status_t Write(uint64_t addr, const IoValue& value) override;
  std::string_view Name() const override { return "PL011"; }

  // |PlatformDevice|
  zx_status_t ConfigureZbi(cpp20::span<std::byte> zbi) const override;
  zx_status_t ConfigureDtb(void* dtb) const override;

 private:
  static constexpr size_t kBufferSize = 128;
  mutable std::mutex mutex_;
  zx::socket socket_;

  uint8_t tx_buffer_[kBufferSize] __TA_GUARDED(mutex_) = {};
  uint16_t tx_offset_ __TA_GUARDED(mutex_) = 0;
  uint16_t control_ __TA_GUARDED(mutex_) = 0;
  uint16_t lcr_ __TA_GUARDED(mutex_) = 0;

  void Print(uint8_t ch);
};

#endif  // SRC_VIRTUALIZATION_BIN_VMM_ARCH_ARM64_PL011_H_
