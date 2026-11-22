// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/memory_barriers/memory_barriers.h>
#include <lib/trace/event.h>

#include <algorithm>
#include <bit>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"

namespace dwc3 {

namespace {

// The valid range of `DEPCFG.bInterval_m1` is 0..13: the core's microframe counter (`DSTS.SOFFN`)
// is 14 bits wide, so periodic intervals of 2^14 or 2^15 microframes cannot be tracked.
constexpr uint8_t kMaxDepcfgIntervalM1 = 13;

// Converts a USB endpoint descriptor's `bInterval` value into the 0-based `bInterval_m1` exponent
// expected by DWC3 `DEPCFG` Parameter 1 bits [23:16] ($2^{\text{bInterval\_m1}}$ frames or
// microframes).
uint8_t ComputeDepcfgInterval(fuchsia_hardware_usb_descriptor::EndpointType ep_type,
                              uint8_t interval, fuchsia_hardware_usb_descriptor::UsbSpeed speed) {
  using fuchsia_hardware_usb_descriptor::EndpointType;
  using fuchsia_hardware_usb_descriptor::UsbSpeed;

  switch (ep_type) {
    case EndpointType::kIsochronous:
      return interval > 0 ? std::min<uint8_t>(interval - 1, kMaxDepcfgIntervalM1) : 0;
    case EndpointType::kInterrupt:
      if (interval == 0) {
        return 0;
      }
      if (speed == UsbSpeed::kFull || speed == UsbSpeed::kLow) {
        // Full/Low-speed interrupt `bInterval` is specified in linear 1 ms frames (1..255).
        // Pick the largest power-of-two frame interval <= `interval`.
        return static_cast<uint8_t>(std::bit_width(interval) - 1);
      }
      // High/Super/EnhancedSuper interrupt `bInterval` is 1-based exponent (1..16).
      return std::min<uint8_t>(interval - 1, kMaxDepcfgIntervalM1);
    case EndpointType::kControl:
    case EndpointType::kBulk:
    default:
      return 0;
  }
}

// Per Synopsys DWC_usb3 Programming Guide 3.2.2 and Linux dwc3_send_gadget_ep_cmd(), clears
// GUSB2PHYCFG.SUSPENDUSB20 and ENBLSLPM while an endpoint command executes so the USB 2.0 PHY
// clock is not gated, and restores the saved value afterward. The register is only written when
// either bit was set.
class ScopedUsb2PhyClockUngate {
 public:
  explicit ScopedUsb2PhyClockUngate(fdf::MmioBuffer* mmio) : mmio_(mmio) {
    auto gusb2phycfg = GUSB2PHYCFG::Get(0).ReadFrom(mmio_);
    saved_gusb2phycfg_ = gusb2phycfg.reg_value();
    restore_ = gusb2phycfg.SUSPENDUSB20() || gusb2phycfg.ENBLSLPM();
    if (restore_) {
      gusb2phycfg.set_SUSPENDUSB20(0).set_ENBLSLPM(0).WriteTo(mmio_);
    }
  }

  ~ScopedUsb2PhyClockUngate() {
    if (restore_) {
      GUSB2PHYCFG::Get(0).FromValue(saved_gusb2phycfg_).WriteTo(mmio_);
    }
  }

  ScopedUsb2PhyClockUngate(const ScopedUsb2PhyClockUngate&) = delete;
  ScopedUsb2PhyClockUngate& operator=(const ScopedUsb2PhyClockUngate&) = delete;

 private:
  fdf::MmioBuffer* const mmio_;
  uint32_t saved_gusb2phycfg_ = 0;
  bool restore_ = false;
};

}  // namespace

static constexpr uint32_t kEarlyLoopExitCount = 100000;

// Spin wait for a command to complete with an early exit if stuck.
bool Dwc3::WaitForCmdAct(const char* caller_name, const uint8_t ep_num) {
  TRACE_DURATION("dwc3", "Dwc3::WaitForCmdAct", "caller", caller_name);

  auto* mmio = get_mmio();
  uint32_t loop_count = 0;

  while (true) {
    loop_count++;
    if (loop_count >= kEarlyLoopExitCount) {
      fdf::error("Dwc3::WaitForCmdAct() Forced exit from spin loop for {:s} for ep{}", caller_name,
                 ep_num);
      metrics_.RecordCmdActTimeout();
      return false;
    }
    if (!DEPCMD::Get(ep_num).ReadFrom(mmio).CMDACT()) {
      return true;
    }
  }
}

bool Dwc3::CmdStartNewConfig(const Endpoint& ep, uint32_t rsrc_id_base) {
  TRACE_DURATION("dwc3", "Dwc3::CmdStartNewConfig", "ep_num", ep.ep_num, "rsrc_id_base",
                 rsrc_id_base);

  // The Start New Configuration specification expects this function to
  // only be called with '0', when setting up EP0 and EP1. After these endpoints
  // are configured and Start Configuration is called, we expect this function
  // to be called with '2', for setting up EP2 and above.
  ZX_DEBUG_ASSERT_MSG(rsrc_id_base == 0 || rsrc_id_base == 2, "%s: rsrc_id_base = %u != {0, 2}",
                      __func__, rsrc_id_base);

  auto* mmio = get_mmio();
  const uint8_t ep_num = ep.ep_num;

  DEPCMDPAR0::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR1::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num)
      .FromValue(0)
      .set_CMDTYP(DEPCMD::DEPSTARTCFG)
      .set_COMMANDPARAM(rsrc_id_base)
      .set_CMDACT(1)
      .WriteTo(mmio);

  return WaitForCmdAct(__func__, ep_num);
}

void Dwc3::CmdEpSetConfig(const Endpoint& ep, bool modify) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpSetConfig", "ep_num", ep.ep_num, "modify", modify);
  auto* mmio = get_mmio();
  const uint8_t ep_num = ep.ep_num;

  // fifo number is zero for OUT endpoints and EP0_IN
  const uint32_t fifo_num = (ep.IsOutput() || (ep_num == kEp0In)) ? 0 : ep_num >> 1;
  const uint32_t action =
      modify ? DEPCFG_DEPCMDPAR0::ACTION_MODIFY : DEPCFG_DEPCMDPAR0::ACTION_INITIALIZE;

  DEPCFG_DEPCMDPAR0::Get(ep_num)
      .FromValue(0)
      .set_FIFO_NUM(fifo_num)
      .set_MAX_PACKET_SIZE(ep.max_packet_size)
      .set_EP_TYPE(fidl::ToUnderlying(ep.type))
      .set_ACTION(action)
      .WriteTo(mmio);
  DEPCFG_DEPCMDPAR1::Get(ep_num)
      .FromValue(0)
      .set_EP_NUMBER(ep_num)
      .set_INTERVAL(ComputeDepcfgInterval(ep.type, ep.interval, connection_speed_))
      .set_XFER_NOT_READY_EN(1)
      .set_XFER_COMPLETE_EN(1)
      // Only pertinent if we're allowing multiple TRBs per transfer.
      .set_XFER_IN_PROGRESS_EN(AllowEnqueueManyTRBs(ep.type))
      .set_INTR_NUM(0)
      .WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num).FromValue(0).set_CMDTYP(DEPCMD::DEPCFG).set_CMDACT(1).WriteTo(mmio);

  WaitForCmdAct(__func__, ep_num);
}

void Dwc3::CmdEpTransferConfig(const Endpoint& ep) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpTransferConfig", "ep_num", ep.ep_num);
  auto* mmio = get_mmio();
  const uint8_t ep_num = ep.ep_num;

  DEPCMDPAR0::Get(ep_num).FromValue(0).set_PARAMETER(1).WriteTo(mmio);
  DEPCMDPAR1::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num).FromValue(0).set_CMDTYP(DEPCMD::DEPXFERCFG).set_CMDACT(1).WriteTo(mmio);

  WaitForCmdAct(__func__, ep_num);
}

void Dwc3::CmdEpStartTransfer(const Endpoint& ep, zx_paddr_t trb_phys) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpStartTransfer", "ep_num", ep.ep_num, "trb_phys", trb_phys);
  auto* mmio = get_mmio();
  const uint8_t ep_num = ep.ep_num;

  // Per the Synopsys DWC3 Databook specification for the StartTransfer command,
  // the 64-bit TRB address must be mapped as follows:
  // - DEPCMDPAR0 (offset 0x08) receives the High Address (bits 63:32)
  // - DEPCMDPAR1 (offset 0x04) receives the Low Address (bits 31:0)
  // This matches little-endian layout where the lower 32-bits sit at the lower offset.
  //
  // The TRBs may live in uncached (device) memory and the DEPCMD write is the doorbell that makes
  // the controller fetch them, so the TRB writes (including HWO) must be complete before the
  // command is issued. Do not rely on any barrier the MMIO accessors may emit after each write.
  WriteDataSynchronizationBarrier();
  DEPCMDPAR0::Get(ep_num)
      .FromValue(0)
      .set_PARAMETER(static_cast<uint32_t>(trb_phys >> 32))
      .WriteTo(mmio);
  DEPCMDPAR1::Get(ep_num).FromValue(0).set_PARAMETER(static_cast<uint32_t>(trb_phys)).WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num)
      .FromValue(0)
      .set_CMDTYP(DEPCMD::DEPSTRTXFER)
      .set_CMDACT(1)
      .set_CMDIOC(1)
      .WriteTo(mmio);

  WaitForCmdAct(__func__, ep_num);
}

bool Dwc3::CmdEpUpdateTransfer(const Endpoint& ep) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpUpdateTransfer", "ep_num", ep.ep_num);
  auto* mmio = get_mmio();
  const uint8_t ep_num = ep.ep_num;

  // Update Transfer tells the controller to fetch the TRBs newly armed on an already started
  // transfer. It takes no parameters, so DEPCMDPAR0-2 are not written.
  //
  // The controller also accepts a "no response" form of this command (CMDACT=0, CMDIOC=0) that it
  // never acknowledges. That form is only valid when software does not rely on XferNotReady
  // events for the endpoint. This driver enables XferNotReady on every endpoint and gates
  // StartTransfer on it, so it issues the normal form instead and waits for the controller to
  // clear CMDACT. With CMDIOC=0 there is no command complete event, so, as in Linux
  // dwc3_send_gadget_ep_cmd(), a rejected command is only reported in DEPCMD.CMDSTATUS once CMDACT
  // clears. Rejected and timed out kicks are logged and reported to the caller, which counts them.
  ZX_DEBUG_ASSERT_MSG(ep.rsrc_id != Endpoint::kInvalidResourceId && ep.rsrc_id <= 0x7fu,
                      "%s: ep%d has no valid transfer resource ID (rsrc_id=0x%08x)", __func__,
                      ep_num, ep.rsrc_id);

  // Writing DEPCMD while CMDACT is still set (a previous command on this endpoint timed out)
  // would overwrite the command the controller may still be executing.
  if (DEPCMD::Get(ep_num).ReadFrom(mmio).CMDACT()) {
    fdf::error("Dwc3::CmdEpUpdateTransfer ep{}: previous command still active, not issuing",
               ep_num);
    return false;
  }

  ScopedUsb2PhyClockUngate usb2_phy_clock(mmio);

  // The TRBs live in uncached (device) memory and the DEPCMD write is the doorbell that makes the
  // controller fetch them, so the TRB writes (including HWO) must be complete before it. Update
  // writes no DEPCMDPARn registers whose MMIO writes might otherwise sit in between.
  WriteDataSynchronizationBarrier();
  DEPCMD::Get(ep_num)
      .FromValue(0)
      .set_CMDTYP(DEPCMD::DEPUPDXFER)
      .set_COMMANDPARAM(ep.rsrc_id)
      .set_CMDACT(1)
      .set_CMDIOC(0)
      .WriteTo(mmio);

  if (!WaitForCmdAct(__func__, ep_num)) {
    return false;
  }

  if (const uint32_t status = DEPCMD::Get(ep_num).ReadFrom(mmio).CMDSTATUS(); status != 0) {
    fdf::error("Dwc3::CmdEpUpdateTransfer ep{} (rsrc_id={}): rejected with CMDSTATUS {}", ep_num,
               ep.rsrc_id, status);
    return false;
  }
  return true;
}

bool Dwc3::CmdEpEndTransfer(const Endpoint& ep, bool cmd_ioc) {
  return CmdEpEndTransfer(ep, ep.rsrc_id, cmd_ioc);
}

bool Dwc3::CmdEpEndTransfer(const Endpoint& ep, uint32_t rsrc_id, bool cmd_ioc) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpEndTransfer", "ep_num", ep.ep_num);
  if (!power_on_) {
    return false;
  }

  auto* mmio = get_mmio();

  const uint8_t ep_num = ep.ep_num;

  ZX_DEBUG_ASSERT_MSG(rsrc_id != Endpoint::kInvalidResourceId,
                      "%s: Called before rsrc_id was initialized with a valid value "
                      "ep.ep_num=%d ep.enabled=%d ep.type=%d ep.transfer_state=%d "
                      "ep.stalled=%d rsrc_id=0x%08x",
                      __func__, ep_num, ep.enabled, static_cast<uint8_t>(ep.type),
                      ep.transfer_state, ep.stalled, rsrc_id);

  ScopedUsb2PhyClockUngate usb2_phy_clock(mmio);

  DEPCMDPAR0::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR1::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num)
      .FromValue(0)
      .set_CMDTYP(DEPCMD::DEPENDXFER)
      .set_COMMANDPARAM(rsrc_id)
      .set_CMDACT(1)
      .set_CMDIOC(cmd_ioc ? 1 : 0)
      .set_HIPRI_FORCERM(1)
      .WriteTo(mmio);

  const bool ok = WaitForCmdAct(__func__, ep_num);
  if (!cmd_ioc) {
    // When CMDIOC is not set, no CommandComplete event will be generated; give the hardware time to
    // finish halting any in-flight DMA burst before software disables DALEPENA or unmaps memory.
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  return ok;
}

void Dwc3::CmdEpSetStall(const Endpoint& ep) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpSetStall", "ep_num", ep.ep_num);
  auto* mmio = get_mmio();

  const uint8_t ep_num = ep.ep_num;

  DEPCMDPAR0::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR1::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num)
      .FromValue(0)
      .set_CMDTYP(DEPCMD::DEPSSTALL)
      .set_CMDACT(1)
      .set_CMDIOC(0)
      .WriteTo(mmio);

  WaitForCmdAct(__func__, ep_num);
}

void Dwc3::CmdEpClearStall(const Endpoint& ep) {
  TRACE_DURATION("dwc3", "Dwc3::CmdEpClearStall", "ep_num", ep.ep_num);
  auto* mmio = get_mmio();

  const uint8_t ep_num = ep.ep_num;
  // Per Synopsys STAR 9000614252, set ClearPendIN when clearing stall on SuperSpeed+ IN endpoints
  // so any stale ERDY/pending IN state is cleared.
  const bool clear_pend_in =
      ep.IsInput() && (connection_speed_ == fdescriptor::UsbSpeed::kSuper ||
                       connection_speed_ == fdescriptor::UsbSpeed::kEnhancedSuper);

  DEPCMDPAR0::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR1::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMDPAR2::Get(ep_num).FromValue(0).WriteTo(mmio);
  DEPCMD::Get(ep_num)
      .FromValue(0)
      .set_CMDTYP(DEPCMD::DEPCSTALL)
      .set_CLEARPENDIN(clear_pend_in ? 1 : 0)
      .set_CMDACT(1)
      .set_CMDIOC(0)
      .WriteTo(mmio);

  WaitForCmdAct(__func__, ep_num);
}

}  // namespace dwc3
