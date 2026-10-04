// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_CLOCK_DRIVERS_VIM3_CLK_MESON_GATE_H_
#define SRC_DEVICES_CLOCK_DRIVERS_VIM3_CLK_MESON_GATE_H_

#include <lib/driver/mmio/cpp/mmio.h>
#include <zircon/assert.h>

#include <cstdint>
#include <optional>

namespace vim3_clock {

enum class RegisterBank { Hiu, Dos };

using meson_gate_descriptor_t = struct meson_gate_descriptor {
  const uint32_t id;
  const uint32_t offset;
  const uint32_t mask;
  const RegisterBank bank;
  const uint32_t hiu_reg = 0;
  const uint32_t hiu_mask = 0;
  const uint32_t hiu_enable_val = 0;
  const uint32_t hiu_disable_val = 0;
};

class MesonGate {
 public:
  MesonGate(const meson_gate_descriptor_t& desc, const fdf::MmioView& mmio,
            const fdf::MmioView& hiu_mmio)
      : id_(desc.id),
        offset_(desc.offset),
        mask_(desc.mask),
        mmio_(mmio),
        hiu_mmio_(hiu_mmio),
        hiu_reg_(desc.hiu_reg),
        hiu_mask_(desc.hiu_mask),
        hiu_enable_val_(desc.hiu_enable_val),
        hiu_disable_val_(desc.hiu_disable_val) {}

  void Enable();
  void Disable();

 private:
  void EnableHw();
  void DisableHw();

  // Number of times Enable has been called on this clock.
  // Clock is enabled iff `vote_count_` is greater than 0.
  int vote_count_ = 0;

  const uint32_t id_;
  const uint32_t offset_;
  const uint32_t mask_;
  fdf::MmioView mmio_;
  fdf::MmioView hiu_mmio_;
  const uint32_t hiu_reg_;
  const uint32_t hiu_mask_;
  const uint32_t hiu_enable_val_;
  const uint32_t hiu_disable_val_;
};

constexpr uint32_t kG12bHhiSysCpuClkCntl1 = (0x57 << 2);
constexpr uint32_t kG12bHhiSysCpubClkCntl1 = (0x80 << 2);
constexpr uint32_t kG12bHhiSysCpubClkCntl = (0x82 << 2);
constexpr uint32_t kG12bHhiTsClkCntl = (0x64 << 2);
constexpr uint32_t kG12bHhiXtalDivnCntl = (0x2f << 2);
constexpr uint32_t kG12bDosGclkEn0 = (0x3f01 << 2);
constexpr uint32_t kG12bHhiGclkMpeg0 = (0x50 << 2);
constexpr uint32_t kG12bHhiGclkMpeg1 = (0x51 << 2);
constexpr uint32_t kG12bHhiGclkMpeg2 = (0x52 << 2);
constexpr uint32_t kHhiSysCpuClkCntl0 = (0x67 << 2);
constexpr uint32_t kG12bHhiVdecClkCntl = (0x78u << 2u);

// Bitfield definitions for HhiVdecClkCntl (HIU 0x78 << 2)
constexpr uint32_t kHhiVdecClkCntlVdecDivMask = (0x7Fu << 0u);
constexpr uint32_t kHhiVdecClkCntlVdecEnMask = (0x1u << 8u);
constexpr uint32_t kHhiVdecClkCntlVdecSelMask = (0x7u << 9u);
constexpr uint32_t kHhiVdecClkCntlVdecMask =
    kHhiVdecClkCntlVdecDivMask | kHhiVdecClkCntlVdecEnMask | kHhiVdecClkCntlVdecSelMask;
constexpr uint32_t kHhiVdecClkCntlVdecEnableVal = (1u << 8u) | (2u << 9u);
constexpr uint32_t kHhiVdecClkCntlVdecDisableVal = 0;

constexpr uint32_t kHhiVdecClkCntlHcodecDivMask = (0x7Fu << 16u);
constexpr uint32_t kHhiVdecClkCntlHcodecEnMask = (0x1u << 24u);
constexpr uint32_t kHhiVdecClkCntlHcodecSelMask = (0x7u << 25u);
constexpr uint32_t kHhiVdecClkCntlHcodecMask =
    kHhiVdecClkCntlHcodecDivMask | kHhiVdecClkCntlHcodecEnMask | kHhiVdecClkCntlHcodecSelMask;
constexpr uint32_t kHhiVdecClkCntlHcodecEnableVal = (1u << 24u) | (2u << 25u);
constexpr uint32_t kHhiVdecClkCntlHcodecDisableVal = 0;

// clang-format off
inline constexpr meson_gate_descriptor_t kGateDescriptors[] = {
  // Sys CPU Clock Gates
  {.id = 0,   .offset = kG12bHhiSysCpuClkCntl1,  .mask=(1 << 24),        .bank=RegisterBank::Hiu},
  {.id = 1,   .offset = kG12bHhiSysCpuClkCntl1,  .mask=(1 << 1),         .bank=RegisterBank::Hiu},
  {.id = 2,   .offset = kG12bHhiXtalDivnCntl,    .mask=(1 << 11),        .bank=RegisterBank::Hiu},

  // Sys CPUB Clock Gates
  {.id = 3,   .offset = kG12bHhiSysCpubClkCntl1, .mask=(1 << 24),        .bank=RegisterBank::Hiu},
  {.id = 4,   .offset = kG12bHhiSysCpubClkCntl1, .mask=(1 << 1),         .bank=RegisterBank::Hiu},

  // Graphics
  {.id = 5,   .offset = kG12bDosGclkEn0,         .mask= 0x3ff,           .bank=RegisterBank::Dos,
   .hiu_reg = kG12bHhiVdecClkCntl, .hiu_mask = kHhiVdecClkCntlVdecMask,
   .hiu_enable_val = kHhiVdecClkCntlVdecEnableVal, .hiu_disable_val = kHhiVdecClkCntlVdecDisableVal,},
  {.id = 6,   .offset = kG12bDosGclkEn0,         .mask=(0x7fffu << 12u), .bank=RegisterBank::Dos,
   .hiu_reg = kG12bHhiVdecClkCntl, .hiu_mask = kHhiVdecClkCntlHcodecMask,
   .hiu_enable_val = kHhiVdecClkCntlHcodecEnableVal, .hiu_disable_val = kHhiVdecClkCntlHcodecDisableVal,},

  // MPeg 0 DOS
  {.id = 7,   .offset = kG12bHhiGclkMpeg0,       .mask=(1 << 1),         .bank=RegisterBank::Hiu},

  // USB Gates
  {.id = 8,   .offset = kG12bHhiGclkMpeg1,       .mask=(1 << 26),        .bank=RegisterBank::Hiu},
  {.id = 9,   .offset = kG12bHhiGclkMpeg2,       .mask=(1 << 8),         .bank=RegisterBank::Hiu},


  {.id = 10,  .offset = kG12bHhiXtalDivnCntl,    .mask=(1 << 12),        .bank=RegisterBank::Hiu},

  {.id = 11,  .offset = kG12bHhiGclkMpeg1,       .mask=(1 << 0),         .bank=RegisterBank::Hiu},
  {.id = 12,  .offset = kG12bHhiGclkMpeg0,       .mask=(1 << 26),        .bank=RegisterBank::Hiu},

  // Temp Sensors
  {.id = 13,  .offset = kG12bHhiTsClkCntl,       .mask=(1 << 8),         .bank=RegisterBank::Hiu},

};
// clang-format on

}  // namespace vim3_clock

#endif  // SRC_DEVICES_CLOCK_DRIVERS_VIM3_CLK_MESON_GATE_H_
