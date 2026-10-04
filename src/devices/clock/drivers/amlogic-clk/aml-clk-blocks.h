// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_CLOCK_DRIVERS_AMLOGIC_CLK_AML_CLK_BLOCKS_H_
#define SRC_DEVICES_CLOCK_DRIVERS_AMLOGIC_CLK_AML_CLK_BLOCKS_H_

#include <zircon/types.h>

#include <soc/aml-s905d2/s905d2-hiu.h>

// MMIO ranges that can contain clock gates.
enum meson_register_sets {
  // HIU is the default set of registers.
  kMesonRegisterSetHiu = 0,
  kMesonRegisterSetDos,
};

// Bitfield definitions for HhiVdecClkCntl (HIU 0x78 << 2)
//
// On G12A, G12B, and SM1 (G12x), the clock source mux inputs (vdec_sel and hcodec_sel) are:
//   0: fclk_div2p5 (800 MHz)
//   1: fclk_div3   (666.7 MHz)
//   2: fclk_div4   (500 MHz)
//   3: fclk_div5   (400 MHz)
//   4: fclk_div7   (285.7 MHz)
//   5: hifi_pll
//   6: gp0_pll
//   7: xtal        (24 MHz)
// (Note: On older GXM/S912, selector 0 was fclk_div4 (500 MHz) and selector 1 was fclk_div3
// (666.7 MHz), whereas on G12x selector 2 is fclk_div4 (500 MHz).)

// VDEC (Decoder) clock control:
constexpr uint32_t kHhiVdecClkCntl = (0x78u << 2u);
constexpr uint32_t kHhiVdecClkCntlVdecDivMask = (0x7Fu << 0u);
constexpr uint32_t kHhiVdecClkCntlVdecEnMask = (0x1u << 8u);
constexpr uint32_t kHhiVdecClkCntlVdecSelMask = (0x7u << 9u);
constexpr uint32_t kHhiVdecClkCntlVdecMask =
    kHhiVdecClkCntlVdecDivMask | kHhiVdecClkCntlVdecEnMask | kHhiVdecClkCntlVdecSelMask;

// Intentional 500 MHz (vdec_sel = 2, fclk_div4) vs 666.7 MHz (vdec_sel = 1, fclk_div3):
// Per the comment previously in vdec1.cc (and in hevcdec.cc), the maximum frequency used in Linux
// is 648 MHz, which requires using GP0 (already used by the GPU). Although VDEC1 was previously
// observed to run up to 800 MHz after fixing earlier clock-related glitches, 500 MHz (fclk_div4)
// is plenty for now and it is prudent to run at <= the 648 MHz max frequency used on Linux, just
// in case (whereas fclk_div3 at 666.7 MHz would exceed 648 MHz).
//
// vdec_en = 1 (bit 8), vdec_sel = 2 (fclk_div4 = 500 MHz, bits 11:9), vdec_div = 0 (div by 1)
constexpr uint32_t kHhiVdecClkCntlVdecEnableVal = (1u << 8u) | (2u << 9u);
constexpr uint32_t kHhiVdecClkCntlVdecDisableVal = 0;

// HCODEC (Encoder) clock control:
constexpr uint32_t kHhiVdecClkCntlHcodecDivMask = (0x7Fu << 16u);
constexpr uint32_t kHhiVdecClkCntlHcodecEnMask = (0x1u << 24u);
constexpr uint32_t kHhiVdecClkCntlHcodecSelMask = (0x7u << 25u);
constexpr uint32_t kHhiVdecClkCntlHcodecMask =
    kHhiVdecClkCntlHcodecDivMask | kHhiVdecClkCntlHcodecEnMask | kHhiVdecClkCntlHcodecSelMask;

// Intentional 500 MHz (hcodec_sel = 2, fclk_div4) vs 666.7 MHz (hcodec_sel = 1, fclk_div3):
// In amlogic_h264_encoder, 500 MHz (kFclkDiv4 = 2) has been used the longest without triggering
// encoder firmware hangs or resets, whereas reliability at 666.7 MHz (kFclkDiv3 = 1) is unproven.
//
// hcodec_en = 1 (bit 24), hcodec_sel = 2 (fclk_div4 = 500 MHz, bits 27:25), hcodec_div = 0 (div by
// 1)
constexpr uint32_t kHhiVdecClkCntlHcodecEnableVal = (1u << 24u) | (2u << 25u);
constexpr uint32_t kHhiVdecClkCntlHcodecDisableVal = 0;

typedef struct meson_clk_gate {
  uint32_t reg;              // Offset from Clock Base Addr in bytes.
  uint32_t bit;              // Offset into this register.
  uint32_t register_set;     // Index determining which set of registers the clock belongs to.
  uint32_t mask;             // If this is nonzero, |bit| is ignored and this mask is used instead.
  uint32_t hiu_reg;          // Optional secondary HIU register offset (e.g. kHhiVdecClkCntl).
  uint32_t hiu_mask;         // Mask of bits in hiu_reg controlled by this clock.
  uint32_t hiu_enable_val;   // Value for masked bits in hiu_reg on enable.
  uint32_t hiu_disable_val;  // Value for masked bits in hiu_reg on disable.
} meson_clk_gate_t;

typedef struct meson_clk_msr {
  uint32_t reg0_offset;  // Offset of MSR_CLK_REG0 from MSR_CLK Base Addr
  uint32_t reg2_offset;  // Offset of MSR_CLK_REG2 from MSR_CLK Base Addr
} meson_clk_msr_t;

typedef struct meson_clk_mux {
  uint32_t reg;            // Offset from Clock Base in bytes.
  uint32_t mask;           // Right Justified Mask of the mux selection bits.
  uint32_t shift;          // Offset of the Mux input index in the register in bits.
  uint32_t n_inputs;       // Number of possible inputs to select from.
  const uint32_t *inputs;  // If set, this field maps indicies to mux selection values
                           // since indices must always be in the range [0, n_inputs).
} meson_clk_mux_t;

typedef struct meson_cpu_clk {
  uint32_t reg;
  hhi_plls_t pll;
  uint32_t initial_hz;
} meson_cpu_clk_t;

#endif  // SRC_DEVICES_CLOCK_DRIVERS_AMLOGIC_CLK_AML_CLK_BLOCKS_H_
