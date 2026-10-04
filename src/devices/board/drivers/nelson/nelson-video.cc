// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/ddk/binding.h>
#include <lib/ddk/debug.h>
#include <lib/ddk/device.h>
#include <lib/ddk/platform-defs.h>
#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <zircon/syscalls/smc.h>

#include <bind/fuchsia/amlogic/platform/meson/cpp/bind.h>
#include <bind/fuchsia/cpp/bind.h>
#include <soc/aml-meson/sm1-clk.h>
#include <soc/aml-s905d3/s905d3-hw.h>

#include "nelson.h"

namespace nelson {
namespace fpbus = fuchsia_hardware_platform_bus;

constexpr uint64_t kCbusResetOffset = 0x1000;
constexpr uint64_t kCbusResetLength = 0x1000;
constexpr uint64_t kCbusDemuxOffset = 0x6000;
constexpr uint64_t kCbusDemuxLength = 0x1000;
constexpr uint64_t kCbusParserOffset = 0xe000;
constexpr uint64_t kCbusParserLength = 0x1000;
constexpr uint64_t kHiubusLength = 0x1000;
constexpr uint64_t kAobusLength = 0x1000;

static const std::vector<fpbus::Mmio> nelson_video_mmios{
    {{
        // kCbusReset (CBUS Page 1: Reset registers at +0x400 offset)
        .base = S905D3_CBUS_BASE + kCbusResetOffset,
        .length = kCbusResetLength,
    }},
    {{
        // kCbusDemux (CBUS Page 6: Demux registers)
        .base = S905D3_CBUS_BASE + kCbusDemuxOffset,
        .length = kCbusDemuxLength,
    }},
    {{
        // kCbusParser (CBUS Page 14: Parser registers)
        .base = S905D3_CBUS_BASE + kCbusParserOffset,
        .length = kCbusParserLength,
    }},
    {{
        .base = S905D3_DOS_BASE,
        .length = S905D3_DOS_LENGTH,
    }},
    {{
        .base = S905D3_HIU_BASE,
        .length = kHiubusLength,
    }},
    {{
        .base = S905D3_AOBUS_BASE,
        .length = kAobusLength,
    }},
    {{
        .base = S905D3_DMC_BASE,
        .length = S905D3_DMC_LENGTH,
    }},
};

static const std::vector<fpbus::Bti> nelson_video_btis{
    {{
        .iommu_id = 0,
        .bti_id = BTI_VIDEO,
    }},
};

static const std::vector<fpbus::Irq> nelson_video_irqs{
    {{
        .irq = fpbus::IrqSpec::WithIrq(S905D3_DEMUX_IRQ),
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
    {{
        .irq = fpbus::IrqSpec::WithIrq(S905D3_PARSER_IRQ),
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
    {{
        .irq = fpbus::IrqSpec::WithIrq(S905D3_DOS_MBOX_0_IRQ),
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
    {{
        .irq = fpbus::IrqSpec::WithIrq(S905D3_DOS_MBOX_1_IRQ),
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
    {{
        .irq = fpbus::IrqSpec::WithIrq(S905D3_DOS_MBOX_2_IRQ),
        .mode = fpbus::ZirconInterruptMode::kEdgeHigh,
    }},
};

static const std::vector<fpbus::Smc> nelson_video_smcs{
    {{
        .service_call_num_base = ARM_SMC_SERVICE_CALL_NUM_TRUSTED_OS_BASE,
        .count = 1,
        .exclusive = false,
    }},
};

static const fpbus::Node video_dev = []() {
  fpbus::Node dev = {};
  dev.name() = "aml_video";
  dev.vid() = PDEV_VID_AMLOGIC;
  dev.pid() = PDEV_PID_AMLOGIC_S905D3;
  dev.did() = PDEV_DID_AMLOGIC_VIDEO;
  dev.mmio() = nelson_video_mmios;
  dev.bti() = nelson_video_btis;
  dev.irq() = nelson_video_irqs;
  dev.smc() = nelson_video_smcs;
  return dev;
}();

zx_status_t Nelson::VideoInit() {
  // We only map the specific pages needed of the HIU, AOBUS, and CBUS register blocks to limit MMIO
  // access privilege. Assert that our mapped ranges do not exceed the hardware register block
  // lengths.
  static_assert(kHiubusLength <= S905D3_HIU_LENGTH);
  static_assert(kAobusLength <= S905D3_AOBUS_LENGTH);
  static_assert(kCbusResetOffset + kCbusResetLength <= S905D3_CBUS_LENGTH);
  static_assert(kCbusDemuxOffset + kCbusDemuxLength <= S905D3_CBUS_LENGTH);
  static_assert(kCbusParserOffset + kCbusParserLength <= S905D3_CBUS_LENGTH);
  fidl::Arena<> fidl_arena;
  fdf::Arena arena('VIDE');
  auto video_canvas = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE,
                                      "fuchsia.hardware.amlogiccanvas.Service"),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.amlogiccanvas.Service"),
          },
  }};

  auto video_clock_dos_vdec = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.clock.Service"),
              fdf::MakeAcceptBindRule(
                  bind_fuchsia::ID,
                  bind_fuchsia_amlogic_platform_meson::SM1_CLK_ID_CLK_DOS_GCLK_VDEC),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.clock.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "clock-dos-vdec"),
          },
  }};

  auto video_clock_dos = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.clock.Service"),
              fdf::MakeAcceptBindRule(bind_fuchsia::ID,
                                      bind_fuchsia_amlogic_platform_meson::SM1_CLK_ID_CLK_DOS),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.clock.Service"),
              fdf::MakeProperty2(bind_fuchsia::NAME, "clock-dos"),
          },
  }};

  auto video_tee = fuchsia_driver_framework::ParentSpec2{{
      .bind_rules =
          {
              fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, "fuchsia.hardware.tee.Service"),
          },
      .properties =
          {
              fdf::MakeProperty2(bind_fuchsia::SERVICE, "fuchsia.hardware.tee.Service"),
          },
  }};

  auto video_spec = fuchsia_driver_framework::CompositeNodeSpec{{
      .name = "aml_video",
      .parents2 = {{video_canvas, video_clock_dos_vdec, video_clock_dos, video_tee}},
  }};

  auto result = pbus_.buffer(arena)->AddCompositeNodeSpec(fidl::ToWire(fidl_arena, video_dev),
                                                          fidl::ToWire(fidl_arena, video_spec));
  if (!result.ok()) {
    zxlogf(ERROR, "%s: AddCompositeNodeSpec Video(video_dev) request failed: %s", __func__,
           result.FormatDescription().data());
    return result.status();
  }
  if (result->is_error()) {
    zxlogf(ERROR, "%s: AddCompositeNodeSpec Video(video_dev) failed: %s", __func__,
           zx_status_get_string(result->error_value()));
    return result->error_value();
  }
  return ZX_OK;
}

}  // namespace nelson
