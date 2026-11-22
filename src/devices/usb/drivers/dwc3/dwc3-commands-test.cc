// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <atomic>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace dwc3 {

class Dwc3CommandsTest : public TestFixture<true> {
 protected:
  std::atomic<uint32_t> depcmd_val_{0};
  std::atomic<uint32_t> last_depcmd_raw_{0};
  std::atomic<uint32_t> depcfg0_val_{0};
  std::atomic<uint32_t> depcfg1_val_{0};
  std::atomic<uint32_t> depcmdpar0_val_{0};
  std::atomic<uint32_t> depcmdpar1_val_{0};
  std::atomic<bool> cmd_written_{false};
  std::atomic<uint32_t> command_param_{0xFFFFFFFFu};

  void MockDepCmd(uint8_t ep_num, std::atomic<bool>* out_written = nullptr, bool timeout = false,
                  std::atomic<uint32_t>* out_command_param = nullptr) {
    dut_.RunInEnvironmentTypeContext(
        [this, ep_num, out_written, timeout, out_command_param](Environment& env) {
          auto& depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
          depcmd.SetReadCallback([this]() -> uint32_t { return depcmd_val_.load(); });
          depcmd.SetWriteCallback(
              [this, out_written, timeout, out_command_param, ep_num](uint64_t val_raw) {
                uint32_t val = static_cast<uint32_t>(val_raw);
                if (out_written) {
                  out_written->store(true, std::memory_order_relaxed);
                }
                last_depcmd_raw_.store(val, std::memory_order_relaxed);
                depcmd_val_.store(timeout ? val : (val & ~(1 << 10)));
                if (out_command_param) {
                  out_command_param->store(DEPCMD::Get(ep_num).FromValue(val).COMMANDPARAM(),
                                           std::memory_order_relaxed);
                }
              });
        });
  }
};

TEST_F(Dwc3CommandsTest, CmdEpSetConfig) {
  MockDepCmd(2);

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcfg0 = env.reg_region()[DEPCFG_DEPCMDPAR0::Get(2).addr()];
    auto& depcfg1 = env.reg_region()[DEPCFG_DEPCMDPAR1::Get(2).addr()];

    depcfg0.SetWriteCallback([this](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcfg0_val_.store(val);
    });
    depcfg1.SetWriteCallback([this](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcfg1_val_.store(val);
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;
    ep.type = fdescriptor::EndpointType::kBulk;
    ep.max_packet_size = 512;
    ep.interval = 0;

    Dwc3TestHelper::CmdEpSetConfig(drv, ep, false);
  });

  // Verify register writes
  auto expected_par0 = DEPCFG_DEPCMDPAR0::Get(2)
                           .FromValue(0)
                           .set_FIFO_NUM(0)
                           .set_MAX_PACKET_SIZE(512)
                           .set_EP_TYPE(2)
                           .set_ACTION(DEPCFG_DEPCMDPAR0::ACTION_INITIALIZE)
                           .reg_value();
  auto expected_par1 = DEPCFG_DEPCMDPAR1::Get(2)
                           .FromValue(0)
                           .set_EP_NUMBER(2)
                           .set_INTERVAL(0)
                           .set_XFER_NOT_READY_EN(1)
                           .set_XFER_COMPLETE_EN(1)
                           .set_XFER_IN_PROGRESS_EN(1)
                           .set_INTR_NUM(0)
                           .reg_value();
  EXPECT_EQ(depcfg0_val_.load(), expected_par0);
  EXPECT_EQ(depcfg1_val_.load(), expected_par1);
}

TEST_F(Dwc3CommandsTest, CmdEpSetConfigInterval) {
  MockDepCmd(2);

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcfg1 = env.reg_region()[DEPCFG_DEPCMDPAR1::Get(2).addr()];
    depcfg1.SetWriteCallback(
        [this](uint64_t val_raw) { depcfg1_val_.store(static_cast<uint32_t>(val_raw)); });
  });

  struct IntervalCase {
    fdescriptor::UsbSpeed speed;
    fdescriptor::EndpointType type;
    uint16_t max_packet_size;
    uint8_t interval;
    uint32_t expected_interval_m1;
  };
  constexpr IntervalCase kCases[] = {
      // High-Speed / SuperSpeed interrupt: 1-based exponent, so bInterval_m1 = bInterval - 1.
      // bInterval = 0 must not underflow, and bInterval >= 15 (2^14+ microframes) exceeds the
      // 14-bit microframe counter and clamps to 13.
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kInterrupt, 512, 0, 0},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kInterrupt, 512, 1, 0},
      {fdescriptor::UsbSpeed::kHigh, fdescriptor::EndpointType::kInterrupt, 512, 4, 3},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kInterrupt, 512, 14, 13},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kInterrupt, 512, 15, 13},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kInterrupt, 512, 16, 13},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kInterrupt, 512, 255, 13},

      // Full-Speed / Low-Speed interrupt: linear 1 ms frame count, so bInterval_m1 =
      // floor(log2(bInterval)), e.g. 10 -> 3 (8 frames) and 255 -> 7 (128 frames).
      {fdescriptor::UsbSpeed::kFull, fdescriptor::EndpointType::kInterrupt, 64, 0, 0},
      {fdescriptor::UsbSpeed::kFull, fdescriptor::EndpointType::kInterrupt, 64, 1, 0},
      {fdescriptor::UsbSpeed::kFull, fdescriptor::EndpointType::kInterrupt, 64, 10, 3},
      {fdescriptor::UsbSpeed::kFull, fdescriptor::EndpointType::kInterrupt, 64, 255, 7},
      {fdescriptor::UsbSpeed::kLow, fdescriptor::EndpointType::kInterrupt, 8, 255, 7},

      // Isochronous: 1-based exponent at every speed, clamped to 13.
      {fdescriptor::UsbSpeed::kHigh, fdescriptor::EndpointType::kIsochronous, 1024, 0, 0},
      {fdescriptor::UsbSpeed::kHigh, fdescriptor::EndpointType::kIsochronous, 1024, 1, 0},
      {fdescriptor::UsbSpeed::kFull, fdescriptor::EndpointType::kIsochronous, 1023, 4, 3},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kIsochronous, 1024, 14, 13},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kIsochronous, 1024, 16, 13},
      {fdescriptor::UsbSpeed::kSuper, fdescriptor::EndpointType::kIsochronous, 1024, 255, 13},

      // Bulk with a non-zero bInterval (e.g. a High-Speed NAK rate) must still program 0.
      {fdescriptor::UsbSpeed::kHigh, fdescriptor::EndpointType::kBulk, 512, 4, 0},
      {fdescriptor::UsbSpeed::kHigh, fdescriptor::EndpointType::kBulk, 512, 255, 0},
  };

  for (const IntervalCase& test_case : kCases) {
    SCOPED_TRACE(testing::Message() << "speed=" << fidl::ToUnderlying(test_case.speed)
                                    << " type=" << +fidl::ToUnderlying(test_case.type)
                                    << " bInterval=" << +test_case.interval);
    depcfg1_val_.store(0xFFFFFFFFu);
    dut_.RunInDriverContext([&](Dwc3& drv) {
      Dwc3TestHelper::SetConnectionSpeed(drv, test_case.speed);
      auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
      ASSERT_NE(uep, nullptr);
      auto& ep = uep->ep;
      ep.type = test_case.type;
      ep.max_packet_size = test_case.max_packet_size;
      ep.interval = test_case.interval;
      Dwc3TestHelper::CmdEpSetConfig(drv, ep, false);
    });
    const auto depcfg1 = DEPCFG_DEPCMDPAR1::Get(2).FromValue(depcfg1_val_.load());
    EXPECT_EQ(depcfg1.INTERVAL(), test_case.expected_interval_m1);
    if (test_case.type == fdescriptor::EndpointType::kInterrupt) {
      EXPECT_EQ(depcfg1.XFER_IN_PROGRESS_EN(), 0u);
    }
  }
}

TEST_F(Dwc3CommandsTest, CmdEpStartTransfer) {
  MockDepCmd(2);

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmdpar0 = env.reg_region()[DEPCMDPAR0::Get(2).addr()];
    auto& depcmdpar1 = env.reg_region()[DEPCMDPAR1::Get(2).addr()];

    depcmdpar0.SetWriteCallback([this](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmdpar0_val_.store(val);
    });
    depcmdpar1.SetWriteCallback([this](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmdpar1_val_.store(val);
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;

    // Use a specific address to verify splitting.
    uint64_t phys_addr = 0x1234567890ABCDEF;
    Dwc3TestHelper::CmdEpStartTransfer(drv, ep, phys_addr);
  });

  // Per the Synopsys DWC3 Databook specification for the StartTransfer command,
  // the 64-bit TRB address must be mapped as follows:
  // - DEPCMDPAR0 (offset 0x08) receives the High Address (bits 63:32)
  // - DEPCMDPAR1 (offset 0x04) receives the Low Address (bits 31:0)
  // This matches little-endian layout where the lower 32-bits sit at the lower offset.
  // 0x1234567890ABCDEF -> High Address: 0x12345678, Low Address: 0x90ABCDEF
  EXPECT_EQ(depcmdpar0_val_.load(), 0x12345678u);
  EXPECT_EQ(depcmdpar1_val_.load(), 0x90ABCDEFu);
}

TEST_F(Dwc3CommandsTest, CmdEpEndTransfer) {
  MockDepCmd(2, &cmd_written_);

  const uint32_t initial_phycfg =
      GUSB2PHYCFG::Get(0).FromValue(0).set_SUSPENDUSB20(1).set_ENBLSLPM(1).reg_value();
  auto phycfg_val = std::make_shared<std::atomic<uint32_t>>(initial_phycfg);
  auto phycfg_during_cmd = std::make_shared<std::atomic<uint32_t>>(initial_phycfg);

  dut_.RunInEnvironmentTypeContext([this, phycfg_val, phycfg_during_cmd](Environment& env) {
    auto& phycfg = env.reg_region()[GUSB2PHYCFG::Get(0).addr()];
    phycfg.SetReadCallback([phycfg_val]() -> uint64_t { return phycfg_val->load(); });
    phycfg.SetWriteCallback(
        [phycfg_val](uint64_t val_raw) { phycfg_val->store(static_cast<uint32_t>(val_raw)); });

    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetReadCallback([this]() -> uint32_t { return depcmd_val_.load(); });
    depcmd.SetWriteCallback([this, phycfg_val, phycfg_during_cmd](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      cmd_written_.store(true, std::memory_order_relaxed);
      phycfg_during_cmd->store(phycfg_val->load());
      depcmd_val_.store(val & ~(1 << 10));
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;
    ep.rsrc_id = 1;  // Valid resource ID

    Dwc3TestHelper::SetPowerOn(drv, true);
    Dwc3TestHelper::CmdEpEndTransfer(drv, ep);
  });

  EXPECT_TRUE(cmd_written_.load());
  EXPECT_EQ(GUSB2PHYCFG::Get(0).FromValue(phycfg_during_cmd->load()).SUSPENDUSB20(), 0u);
  EXPECT_EQ(GUSB2PHYCFG::Get(0).FromValue(phycfg_during_cmd->load()).ENBLSLPM(), 0u);
  EXPECT_EQ(phycfg_val->load(), initial_phycfg);
}

TEST_F(Dwc3CommandsTest, UserEpResetGuardsInvalidRsrc) {
  MockDepCmd(2, &cmd_written_);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;
    ep.transfer_state = Dwc3TestHelper::TransferState::kStartingSingle;
    ep.rsrc_id = 0xFFFFFFFFu;  // kInvalidResourceId

    Dwc3TestHelper::SetPowerOn(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::UserEpReset(drv, *uep);
  });

  // Should NOT write to register if resource ID is invalid.
  EXPECT_FALSE(cmd_written_.load());
}

TEST_F(Dwc3CommandsTest, CmdEpEndTransferNoIoc) {
  auto depcmd_written_val = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext([this, depcmd_written_val](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetReadCallback([this]() -> uint32_t { return depcmd_val_.load(); });
    depcmd.SetWriteCallback([this, depcmd_written_val](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      cmd_written_.store(true, std::memory_order_relaxed);
      depcmd_written_val->store(val, std::memory_order_relaxed);
      depcmd_val_.store(val & ~(1 << 10));
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;
    ep.rsrc_id = 3;

    Dwc3TestHelper::SetPowerOn(drv, true);
    EXPECT_TRUE(Dwc3TestHelper::CmdEpEndTransfer(drv, ep, /*cmd_ioc=*/false));
  });

  ASSERT_TRUE(cmd_written_.load());
  const auto cmd = DEPCMD::Get(2).FromValue(depcmd_written_val->load());
  EXPECT_EQ(cmd.CMDTYP(), DEPCMD::DEPENDXFER);
  EXPECT_EQ(cmd.COMMANDPARAM(), 3u);
  EXPECT_EQ(cmd.CMDIOC(), 0u);
  EXPECT_EQ(cmd.HIPRI_FORCERM(), 1u);
}

// Update Transfer is issued as an acknowledged command without parameters, polled until the
// controller clears CMDACT, and keeps the USB 2.0 PHY clock ungated while it executes.
TEST_F(Dwc3CommandsTest, CmdEpUpdateTransfer) {
  constexpr uint32_t kCmdActBusyReads = 3;
  const uint32_t initial_phycfg =
      GUSB2PHYCFG::Get(0).FromValue(0).set_SUSPENDUSB20(1).set_ENBLSLPM(1).reg_value();
  auto phycfg_val = std::make_shared<std::atomic<uint32_t>>(initial_phycfg);
  auto phycfg_during_cmd = std::make_shared<std::atomic<uint32_t>>(initial_phycfg);
  auto depcmd_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto busy_reads_remaining = std::make_shared<std::atomic<uint32_t>>(0);
  auto reads_after_write = std::make_shared<std::atomic<uint32_t>>(0);
  auto par_writes = std::make_shared<std::atomic<uint32_t>>(0);

  dut_.RunInEnvironmentTypeContext([this, phycfg_val, phycfg_during_cmd, depcmd_writes,
                                    busy_reads_remaining, reads_after_write,
                                    par_writes](Environment& env) {
    auto& phycfg = env.reg_region()[GUSB2PHYCFG::Get(0).addr()];
    phycfg.SetReadCallback([phycfg_val]() -> uint64_t { return phycfg_val->load(); });
    phycfg.SetWriteCallback(
        [phycfg_val](uint64_t val_raw) { phycfg_val->store(static_cast<uint32_t>(val_raw)); });

    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetReadCallback([depcmd_writes, busy_reads_remaining, reads_after_write]() -> uint32_t {
      if (depcmd_writes->load() > 0) {
        reads_after_write->fetch_add(1);
      }
      if (busy_reads_remaining->load() > 0) {
        busy_reads_remaining->fetch_sub(1);
        return DEPCMD::Get(2).FromValue(0).set_CMDACT(1).reg_value();
      }
      return 0;
    });
    depcmd.SetWriteCallback([this, phycfg_val, phycfg_during_cmd, depcmd_writes,
                             busy_reads_remaining](uint64_t val_raw) {
      const uint32_t val = static_cast<uint32_t>(val_raw);
      last_depcmd_raw_.store(val);
      phycfg_during_cmd->store(phycfg_val->load());
      depcmd_writes->fetch_add(1);
      busy_reads_remaining->store(DEPCMD::Get(2).FromValue(val).CMDACT() ? kCmdActBusyReads : 0);
    });

    for (uint32_t addr :
         {DEPCMDPAR0::Get(2).addr(), DEPCMDPAR1::Get(2).addr(), DEPCMDPAR2::Get(2).addr()}) {
      env.reg_region()[addr].SetWriteCallback([par_writes](uint64_t) { par_writes->fetch_add(1); });
    }
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.rsrc_id = 5;
    EXPECT_TRUE(Dwc3TestHelper::CmdEpUpdateTransfer(drv, uep->ep));
  });

  ASSERT_EQ(depcmd_writes->load(), 1u);
  const auto cmd = DEPCMD::Get(2).FromValue(last_depcmd_raw_.load());
  EXPECT_EQ(cmd.CMDTYP(), DEPCMD::DEPUPDXFER);
  EXPECT_EQ(cmd.CMDACT(), 1u);
  EXPECT_EQ(cmd.CMDIOC(), 0u);
  EXPECT_EQ(cmd.COMMANDPARAM(), 5u);
  EXPECT_EQ(par_writes->load(), 0u);
  // Polled DEPCMD until CMDACT cleared.
  EXPECT_EQ(busy_reads_remaining->load(), 0u);
  EXPECT_GT(reads_after_write->load(), kCmdActBusyReads);
  EXPECT_EQ(GUSB2PHYCFG::Get(0).FromValue(phycfg_during_cmd->load()).SUSPENDUSB20(), 0u);
  EXPECT_EQ(GUSB2PHYCFG::Get(0).FromValue(phycfg_during_cmd->load()).ENBLSLPM(), 0u);
  EXPECT_EQ(phycfg_val->load(), initial_phycfg);
}

// With CMDIOC=0 there is no command complete event, so a rejection is only visible in
// DEPCMD.CMDSTATUS once CMDACT clears.
TEST_F(Dwc3CommandsTest, CmdEpUpdateTransferRejected) {
  constexpr uint32_t kNoResource = 1;
  dut_.RunInEnvironmentTypeContext([this](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetReadCallback([this]() -> uint32_t { return depcmd_val_.load(); });
    depcmd.SetWriteCallback([this](uint64_t val_raw) {
      cmd_written_.store(true);
      depcmd_val_.store(DEPCMD::Get(2)
                            .FromValue(static_cast<uint32_t>(val_raw))
                            .set_CMDACT(0)
                            .set_CMDSTATUS(kNoResource)
                            .reg_value());
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.rsrc_id = 5;
    EXPECT_FALSE(Dwc3TestHelper::CmdEpUpdateTransfer(drv, uep->ep));
  });

  EXPECT_TRUE(cmd_written_.load());
}

// A previous command whose CMDACT never cleared may still be executing; Update Transfer must not
// overwrite it.
TEST_F(Dwc3CommandsTest, CmdEpUpdateTransferPreviousCommandActive) {
  depcmd_val_.store(DEPCMD::Get(2).FromValue(0).set_CMDACT(1).reg_value());
  MockDepCmd(2, &cmd_written_);

  uint64_t timeouts_before = 0;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.rsrc_id = 5;
    timeouts_before = Dwc3TestHelper::GetCmdActTimeouts(drv);
    EXPECT_FALSE(Dwc3TestHelper::CmdEpUpdateTransfer(drv, uep->ep));
    EXPECT_EQ(Dwc3TestHelper::GetCmdActTimeouts(drv), timeouts_before);
  });

  EXPECT_FALSE(cmd_written_.load());
}

TEST_F(Dwc3CommandsTest, CmdEpUpdateTransferTimeout) {
  MockDepCmd(2, &cmd_written_, /*timeout=*/true);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.rsrc_id = 5;
    const uint64_t timeouts_before = Dwc3TestHelper::GetCmdActTimeouts(drv);
    EXPECT_FALSE(Dwc3TestHelper::CmdEpUpdateTransfer(drv, uep->ep));
    EXPECT_EQ(Dwc3TestHelper::GetCmdActTimeouts(drv), timeouts_before + 1);
  });

  EXPECT_TRUE(cmd_written_.load());
}

TEST_F(Dwc3CommandsTest, CmdEpSetStall) {
  MockDepCmd(2, &cmd_written_);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;

    Dwc3TestHelper::CmdEpSetStall(drv, ep);
  });

  ASSERT_TRUE(cmd_written_.load());
  const auto cmd = DEPCMD::Get(2).FromValue(last_depcmd_raw_.load());
  EXPECT_EQ(cmd.CMDTYP(), DEPCMD::DEPSSTALL);
  EXPECT_EQ(cmd.CMDACT(), 1u);
  EXPECT_EQ(cmd.CMDIOC(), 0u);
}

TEST_F(Dwc3CommandsTest, CmdEpClearStall) {
  MockDepCmd(2, &cmd_written_);
  MockDepCmd(3);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* out_uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    auto* in_uep = Dwc3TestHelper::GetUserEndpoint(drv, 3);
    ASSERT_NE(out_uep, nullptr);
    ASSERT_NE(in_uep, nullptr);

    Dwc3TestHelper::SetConnectionSpeed(drv, fdescriptor::UsbSpeed::kSuper);
    Dwc3TestHelper::CmdEpClearStall(drv, in_uep->ep);
    const auto ss_in_cmd = DEPCMD::Get(3).FromValue(last_depcmd_raw_.load());
    EXPECT_EQ(ss_in_cmd.CMDTYP(), DEPCMD::DEPCSTALL);
    EXPECT_EQ(ss_in_cmd.CMDACT(), 1u);
    EXPECT_EQ(ss_in_cmd.CMDIOC(), 0u);
    EXPECT_EQ(ss_in_cmd.CLEARPENDIN(), 1u);

    Dwc3TestHelper::CmdEpClearStall(drv, out_uep->ep);
    const auto ss_out_cmd = DEPCMD::Get(2).FromValue(last_depcmd_raw_.load());
    EXPECT_EQ(ss_out_cmd.CMDTYP(), DEPCMD::DEPCSTALL);
    EXPECT_EQ(ss_out_cmd.CMDACT(), 1u);
    EXPECT_EQ(ss_out_cmd.CMDIOC(), 0u);
    EXPECT_EQ(ss_out_cmd.CLEARPENDIN(), 0u);

    Dwc3TestHelper::SetConnectionSpeed(drv, fdescriptor::UsbSpeed::kEnhancedSuper);
    Dwc3TestHelper::CmdEpClearStall(drv, in_uep->ep);
    EXPECT_EQ(DEPCMD::Get(3).FromValue(last_depcmd_raw_.load()).CLEARPENDIN(), 1u);

    Dwc3TestHelper::CmdEpClearStall(drv, out_uep->ep);
    EXPECT_EQ(DEPCMD::Get(2).FromValue(last_depcmd_raw_.load()).CLEARPENDIN(), 0u);

    Dwc3TestHelper::SetConnectionSpeed(drv, fdescriptor::UsbSpeed::kHigh);
    Dwc3TestHelper::CmdEpClearStall(drv, in_uep->ep);
    EXPECT_EQ(DEPCMD::Get(3).FromValue(last_depcmd_raw_.load()).CLEARPENDIN(), 0u);
  });

  EXPECT_TRUE(cmd_written_.load());
}

TEST_F(Dwc3CommandsTest, CmdStartNewConfig) {
  MockDepCmd(2, &cmd_written_, false, &command_param_);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;

    Dwc3TestHelper::CmdStartNewConfig(drv, ep, 2);
  });

  EXPECT_TRUE(cmd_written_.load());
  EXPECT_EQ(command_param_.load(), 2u);
}

TEST_F(Dwc3CommandsTest, CmdEpTransferConfig) {
  MockDepCmd(2, &cmd_written_);

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmdpar0 = env.reg_region()[DEPCMDPAR0::Get(2).addr()];

    depcmdpar0.SetWriteCallback([this](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmdpar0_val_.store(val);
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;

    Dwc3TestHelper::CmdEpTransferConfig(drv, ep);
  });

  EXPECT_TRUE(cmd_written_.load());
  EXPECT_EQ(depcmdpar0_val_.load(), 1u);
}

TEST_F(Dwc3CommandsTest, WaitForCmdActTimeout) {
  MockDepCmd(2, &cmd_written_, true);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    auto& ep = uep->ep;

    // This should trigger the timeout and log a warning, but not hang.
    Dwc3TestHelper::CmdEpTransferConfig(drv, ep);
  });

  EXPECT_TRUE(cmd_written_.load());
}

}  // namespace dwc3
