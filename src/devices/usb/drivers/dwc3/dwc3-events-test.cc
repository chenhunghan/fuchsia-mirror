// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <lib/fit/defer.h>
#include <lib/inspect/cpp/reader.h>

#include <atomic>

#include <gtest/gtest.h>
#include <src/lib/testing/predicates/status.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"
#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"

namespace dwc3 {

constexpr uint32_t MakeDeviceEvent(uint32_t type) {
  return (type << 8) | 1;  // Bit 0 = 1 for non-endpoint device events
}
constexpr uint32_t kEvtDisconnect = MakeDeviceEvent(0);      // DEVT_DISCONNECT
constexpr uint32_t kEvtUsbReset = MakeDeviceEvent(1);        // DEVT_USB_RESET
constexpr uint32_t kEvtConnectionDone = MakeDeviceEvent(2);  // DEVT_CONNECTION_DONE

using Dwc3EventsTest = UnmanagedTestFixture;

// Verifies behavior on a simulated disconnect event.
TEST_F(Dwc3EventsTest, HandleDisconnectEvent) {
  auto cmd_written = std::make_shared<std::atomic<bool>>(false);
  auto depcmd_val = std::make_shared<std::atomic<uint32_t>>(0);

  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto set_connected_called = std::make_shared<std::atomic<bool>>(false);
  auto is_connected_val = std::make_shared<std::atomic<bool>>(true);
  fake_dci.SetSetConnectedCallback([set_connected_called, is_connected_val](bool connected) {
    is_connected_val->store(connected);
    set_connected_called->store(true);
  });

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  auto cleanup_callbacks = DeferClearDepcmdCallbacks(2);

  dut_.RunInEnvironmentTypeContext([depcmd_val, cmd_written](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];

    depcmd.SetReadCallback([depcmd_val]() -> uint32_t { return depcmd_val->load(); });
    depcmd.SetWriteCallback([depcmd_val, cmd_written](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmd_val->store(val & ~(1 << 10));
      cmd_written->store(true);
    });
  });

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                               std::move(endpoints->server), &fake_dci);
    Dwc3TestHelper::BindDciInterface(drv, std::move(endpoints->client));

    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_NE(uep, nullptr);
    if (!uep) {
      return;
    }
    ASSERT_OK(Dwc3TestHelper::InitFifo(drv, 2, false));
    auto* trb = uep->fifo.AdvanceWrite();
    trb->control = TRB_HWO;
    auto& ep = uep->ep;
    ep.transfer_state = Endpoint::TransferState::kActiveOngoing;
    Dwc3TestHelper::SetControllerStarted(drv, true);
    ep.rsrc_id = 1;
    // One TRB was handed to the (fake) controller above.
    ep.queued_trbs = 1;
    uep->server->active_reqs.push(decltype(uep->server->active_reqs)::value_type{
        .request = usb::FidlRequest(
            std::move(fuchsia_hardware_usb_request::Request().defer_completion(false))),
        .total_trbs = 1,
        .completed_trbs = 0,
        .completed_bytes = 0,
    });

    // Simulate DEVT_DISCONNECT event
    Dwc3TestHelper::HandleEvent(drv, kEvtDisconnect);

    EXPECT_EQ(ep.transfer_state, Endpoint::TransferState::kCanceling);
    EXPECT_EQ(ep.rsrc_id, 1u);
  });

  dut_.runtime().RunUntil([set_connected_called]() { return set_connected_called->load(); });

  EXPECT_TRUE(cmd_written->load());
  EXPECT_TRUE(set_connected_called->load());
  EXPECT_FALSE(is_connected_val->load());

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies behavior on a simulated USB reset event.
TEST_F(Dwc3EventsTest, HandleResetEvent) {
  auto depcmd_written = std::make_shared<std::atomic<bool>>(false);
  auto dcfg_written = std::make_shared<std::atomic<bool>>(false);
  auto depcmd_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto dcfg_val = std::make_shared<std::atomic<uint32_t>>(0);

  SetUpAndPowerOnDriver();

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_depcmd = DeferClearDepcmdCallbacks(0);
  auto cleanup_dcfg = fit::defer([this]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DCFG::Get().addr()].SetReadCallback([]() -> uint32_t { return 0; });
      env.reg_region()[DCFG::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext(
      [depcmd_written, dcfg_written, depcmd_val, dcfg_val](Environment& env) {
        auto& depcmd = env.reg_region()[DEPCMD::Get(0).addr()];
        auto& dcfg = env.reg_region()[DCFG::Get().addr()];

        depcmd.SetReadCallback([depcmd_val]() -> uint32_t { return depcmd_val->load(); });
        depcmd.SetWriteCallback([depcmd_written, depcmd_val](uint64_t val_raw) {
          uint32_t val = static_cast<uint32_t>(val_raw);
          depcmd_val->store(val & ~(1 << 10));
          depcmd_written->store(true);
        });

        dcfg.SetReadCallback([dcfg_val]() -> uint32_t { return dcfg_val->load(); });
        dcfg.SetWriteCallback([dcfg_written, dcfg_val](uint64_t val_raw) {
          uint32_t val = static_cast<uint32_t>(val_raw);
          dcfg_val->store(val);
          dcfg_written->store(true);
        });
      });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Simulate DEVT_USB_RESET event
    Dwc3TestHelper::HandleEvent(drv, kEvtUsbReset);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  EXPECT_TRUE(depcmd_written->load());
  EXPECT_TRUE(dcfg_written->load());
  EXPECT_EQ(DCFG::Get().FromValue(dcfg_val->load()).DEVADDR(), 0u);

  TearDownAndPowerOffDriver();
}

// Verifies behavior on a simulated connection done event, including setting the speed.
TEST_F(Dwc3EventsTest, HandleConnectionDoneEvent) {
  auto depcmd_written = std::make_shared<std::atomic<bool>>(false);
  auto setspeed_called = std::make_shared<std::atomic<bool>>(false);
  auto depcmd_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto speed_passed =
      std::make_shared<std::atomic<fuchsia_hardware_usb_descriptor::wire::UsbSpeed>>(
          fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kUndefined);

  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  fake_dci.SetSetSpeedCallback(
      [speed_passed, setspeed_called](fuchsia_hardware_usb_descriptor::wire::UsbSpeed speed) {
        speed_passed->store(speed);
        setspeed_called->store(true);
      });

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  // Use RAII guard to guarantee register callback cleanup on scope exit regardless of assertions.
  auto cleanup_depcmd = DeferClearDepcmdCallbacks(0);
  auto cleanup_dsts = DeferClearDstsCallback();

  dut_.RunInEnvironmentTypeContext([depcmd_written, depcmd_val](Environment& env) {
    auto& dsts = env.reg_region()[DSTS::Get().addr()];
    auto& depcmd = env.reg_region()[DEPCMD::Get(0).addr()];

    // Mock DSTS to return CONNECTSPD_HIGH (0) and simulate successful controller halt to satisfy
    // Spec loops.
    dsts.SetReadCallback([]() -> uint32_t {
      return DSTS::Get()
          .FromValue(0)
          .set_CONNECTSPD(DSTS::CONNECTSPD_HIGH)
          .set_DEVCTRLHLT(1)
          .reg_value();
    });

    depcmd.SetReadCallback([depcmd_val]() -> uint32_t { return depcmd_val->load(); });
    depcmd.SetWriteCallback([depcmd_written, depcmd_val](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      depcmd_val->store(val & ~(1 << 10));
      depcmd_written->store(true);
    });
  });

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                               std::move(endpoints->server), &fake_dci);

    // Bind the client
    Dwc3TestHelper::BindDciInterface(drv, std::move(endpoints->client));

    // Simulate DEVT_CONNECTION_DONE event
    Dwc3TestHelper::HandleEvent(drv, kEvtConnectionDone);
  });

  dut_.runtime().RunUntil([setspeed_called]() { return setspeed_called->load(); });

  EXPECT_TRUE(depcmd_written->load());
  EXPECT_TRUE(setspeed_called->load());
  EXPECT_EQ(speed_passed->load(), fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh);

  // Clean unbind: Zircon's BindServer automatically destroys the server unique_ptr on the
  // dispatcher thread post-unbind!
  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that injecting an unknown event does not crash the driver.
TEST_F(Dwc3EventsTest, HandleUnknownEvent) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Inject an unassigned device event vector (type = 31, which is unhandled in dwc3-events.cc)
    uint32_t event = MakeDeviceEvent(31);

    Dwc3TestHelper::HandleEvent(drv, event);
  });
  // Verify that it doesn't crash!
  TearDownAndPowerOffDriver();
}

// TODO(https://fxbug.dev/538237092): Re-enable once UsbDciInterface::Reset() is added.
// A USB Bus Reset (DEVT_USB_RESET) is an in-band bus signal that resets the device to
// the Default State without dropping physical D+/D- pull-ups. SetConnected(false)
// represents a physical cable unplug and must NOT be called on a bus reset, as doing
// so corrupts usb-peripheral state during host re-enumeration.
//
// Under b/538237092, we are adding a dedicated Reset() method to UsbDciInterface
// (DCI -> usb-peripheral) so DCI can notify usb-peripheral of in-band resets while
// preserving physical connection state (kHostConnected) and cleanly unconfiguring
// active functions via SetConfigured(false).
TEST_F(Dwc3EventsTest, DISABLED_VerifySetConnectedIsNotCalledOnHandleResetEvent) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                               std::move(endpoints->server), &fake_dci);

    // Bind the mock client
    Dwc3TestHelper::BindDciInterface(drv, std::move(endpoints->client));

    // USB Reset (DEVT_USB_RESET) is an in-band bus reset signal received from the host while the
    // device remains physically attached.
    Dwc3TestHelper::HandleEvent(drv, kEvtUsbReset);
  });

  dut_.runtime().RunUntilIdle();

  // 1. Positive Assertion: Verify that SetSpeed WAS invoked on UsbDciInterface to notify
  // DCI of the post-reset link speed re-negotiation.
  EXPECT_TRUE(fake_dci.set_speed_called());

  // 2. Negative Assertion: Verify that SetConnected(false) was NEVER called, preserving physical
  // cable attachment state.
  EXPECT_FALSE(fake_dci.set_connected_called());

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, ResetHwMasksEventInterrupts) {
  SetUpAndPowerOnDriver();

  auto evntintrptmask = std::make_shared<std::atomic<uint32_t>>(0);
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([evntintrptmask](Environment& env) {
    auto& gevntsiz = env.reg_region()[GEVNTSIZ::Get(0).addr()];
    gevntsiz.SetWriteCallback([evntintrptmask](uint64_t val) {
      evntintrptmask->store(
          GEVNTSIZ::Get(0).FromValue(static_cast<uint32_t>(val)).EVNTINTRPTMASK());
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { ASSERT_OK(Dwc3TestHelper::ResetHw(drv, false)); });

  EXPECT_EQ(evntintrptmask->load(), 1u);

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, HandleIrqWhilePoweredOffDoesNotTouchMmio) {
  SetUpAndPowerOnDriver();

  auto mmio_touched = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
    });
  });

  dut_.RunInEnvironmentTypeContext([mmio_touched](Environment& env) {
    auto& gevntsiz = env.reg_region()[GEVNTSIZ::Get(0).addr()];
    gevntsiz.SetWriteCallback([mmio_touched](uint64_t) { mmio_touched->store(true); });

    auto& gevntcount_reg = env.reg_region()[GEVNTCOUNT::Get(0).addr()];
    gevntcount_reg.SetReadCallback([mmio_touched]() -> uint32_t {
      mmio_touched->store(true);
      return 0;
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // When powered off, accessing MMIO can cause external aborts on platforms
    // where clocks/power domains are gated. HandleIrq must not touch MMIO.
    Dwc3TestHelper::SetPowerOn(drv, false);
    Dwc3TestHelper::SetControllerStarted(drv, false);

    Dwc3TestHelper::HandleIrq(drv);
  });

  // Verify that MMIO was not accessed when power_on_ is false.
  EXPECT_FALSE(mmio_touched->load());

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, HandleIrqWhileStoppedMasksEventInterrupts) {
  SetUpAndPowerOnDriver();

  auto evntintrptmask = std::make_shared<std::atomic<uint32_t>>(0);
  auto gevntcount_reads = std::make_shared<std::atomic<uint32_t>>(0);

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
    });
  });

  dut_.RunInEnvironmentTypeContext([evntintrptmask, gevntcount_reads](Environment& env) {
    auto& gevntsiz = env.reg_region()[GEVNTSIZ::Get(0).addr()];
    gevntsiz.SetWriteCallback([evntintrptmask](uint64_t val) {
      evntintrptmask->store(
          GEVNTSIZ::Get(0).FromValue(static_cast<uint32_t>(val)).EVNTINTRPTMASK());
    });

    auto& gevntcount_reg = env.reg_region()[GEVNTCOUNT::Get(0).addr()];
    gevntcount_reg.SetReadCallback([gevntcount_reads]() -> uint32_t {
      gevntcount_reads->fetch_add(1);
      return 0;
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Controller is stopped but core is still powered on (e.g. after StopController()).
    Dwc3TestHelper::SetPowerOn(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, false);

    Dwc3TestHelper::HandleIrq(drv);
  });

  // Verify that GEVNTSIZ interrupt mask was set to 1.
  EXPECT_EQ(evntintrptmask->load(), 1u);
  // Verify that GEVNTCOUNT was not read.
  EXPECT_EQ(gevntcount_reads->load(), 0u);

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, HandleIrqRecordsIrqAndUnhandledIrqMetrics) {
  SetUpAndPowerOnDriver();

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
    });
  });

  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // 1. Canceled interrupt should be ignored and not increment irq_count.
    Dwc3TestHelper::HandleIrq(drv, nullptr, nullptr, ZX_ERR_CANCELED, nullptr);

    // 2. Interrupt while stopped (controller_started_ == false) increments both
    // irq_count and unhandled_irq_count.
    Dwc3TestHelper::SetPowerOn(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, false);
    Dwc3TestHelper::HandleIrq(drv);

    // 3. Interrupt while powered off (power_on_ == false) increments both.
    Dwc3TestHelper::SetPowerOn(drv, false);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::HandleIrq(drv);

    // 4. Spurious interrupt while active (GEVNTCOUNT == 0) increments both.
    Dwc3TestHelper::SetPowerOn(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::HandleIrq(drv);

    inspect::Inspector inspector = Dwc3TestHelper::RecordMetrics(drv);
    auto hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo()).take_value();
    const auto* irq_count = hierarchy.node().get_property<inspect::UintPropertyValue>("irq_count");
    ASSERT_NE(irq_count, nullptr);
    EXPECT_EQ(irq_count->value(), 3u);

    const auto* unhandled_irq_count =
        hierarchy.node().get_property<inspect::UintPropertyValue>("unhandled_irq_count");
    ASSERT_NE(unhandled_irq_count, nullptr);
    EXPECT_EQ(unhandled_irq_count->value(), 3u);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, HandleIrqMasksAcksEarlyAndRearmsInterruptModeration) {
  SetUpAndPowerOnDriver();

  enum class OpKind : uint8_t {
    kGevntsizWrite,
    kGevntcountWrite,
    kDevimodWrite,
    kResetEventDispatched,
  };
  struct RegOp {
    OpKind kind;
    uint32_t value;
  };

  auto ops = std::make_shared<std::vector<RegOp>>();
  auto gevntsiz_reg = std::make_shared<std::atomic<uint32_t>>(
      GEVNTSIZ::Get(0).FromValue(0).set_EVENTSIZ(kBufferSize).set_EVNTINTRPTMASK(0).reg_value());
  auto pending_event_bytes = std::make_shared<std::atomic<uint32_t>>(sizeof(uint32_t));
  auto injected_followup = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_depcmd = DeferClearDepcmdCallbacks(0);
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
      env.reg_region()[DEVIMOD::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[DCFG::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext(
      [ops, gevntsiz_reg, pending_event_bytes, injected_followup](Environment& env) {
        env.reg_region()[DEPCMD::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
        env.reg_region()[GEVNTSIZ::Get(0).addr()].SetReadCallback(
            [gevntsiz_reg]() -> uint32_t { return gevntsiz_reg->load(); });
        env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback(
            [ops, gevntsiz_reg](uint64_t val_raw) {
              const uint32_t val = static_cast<uint32_t>(val_raw);
              gevntsiz_reg->store(val);
              ops->push_back({OpKind::kGevntsizWrite, val});
            });
        env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback(
            [pending_event_bytes]() -> uint32_t { return pending_event_bytes->load(); });
        env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetWriteCallback(
            [ops, pending_event_bytes](uint64_t val_raw) {
              const uint32_t val = static_cast<uint32_t>(val_raw);
              ops->push_back({OpKind::kGevntcountWrite, val});
              const uint32_t acked = GEVNTCOUNT::Get(0).FromValue(val).EVNTCOUNT();
              uint32_t cur = pending_event_bytes->load();
              pending_event_bytes->store(cur >= acked ? cur - acked : 0);
            });
        env.reg_region()[DEVIMOD::Get(0).addr()].SetWriteCallback([ops](uint64_t val) {
          ops->push_back({OpKind::kDevimodWrite, static_cast<uint32_t>(val)});
        });
        // HandleResetEvent writes DCFG(DEVADDR=0) while dispatching DEVT_USB_RESET; record when it
        // runs relative to GEVNTCOUNT acks and simulate a follow-up event arriving mid-dispatch.
        env.reg_region()[DCFG::Get().addr()].SetWriteCallback(
            [ops, pending_event_bytes, injected_followup](uint64_t val) {
              ops->push_back({OpKind::kResetEventDispatched, static_cast<uint32_t>(val)});
              if (!injected_followup->exchange(true)) {
                pending_event_bytes->fetch_add(sizeof(uint32_t));
              }
            });
      });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetPowerOn(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetInterruptModeration(drv, 1000);
    // Pre-populate DEVT_USB_RESET followed by DEVT_SOF in the ring; only the first 4 bytes are
    // initially reported via GEVNTCOUNT, and the second 4 bytes become visible during HandleEvent.
    ASSERT_OK(Dwc3TestHelper::PushEventToFifo(drv, kEvtUsbReset));
    ASSERT_OK(Dwc3TestHelper::PushEventToFifo(drv, MakeDeviceEvent(DEVT_SOF)));

    Dwc3TestHelper::HandleIrq(drv);

    inspect::Inspector inspector = Dwc3TestHelper::RecordMetrics(drv);
    auto hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo()).take_value();
    const auto* max_batch =
        hierarchy.node().get_property<inspect::UintPropertyValue>("max_event_batch_size");
    ASSERT_NE(max_batch, nullptr);
    EXPECT_EQ(max_batch->value(), 2u);
  });

  // Expected sequence with interrupt moderation enabled:
  // 0: GEVNTSIZ(EVNTINTRPTMASK=1)
  // 1: GEVNTCOUNT(EHB=0, EVNTCOUNT=4) -- acknowledged BEFORE HandleResetEvent runs
  // 2: HandleResetEvent DCFG write (which exposes the second 4-byte event in GEVNTCOUNT)
  // 3: GEVNTCOUNT(EHB=0, EVNTCOUNT=4) -- second loop iteration ack
  // 4: GEVNTSIZ(EVNTINTRPTMASK=0) -- unmasked after ring is empty
  // 5: GEVNTCOUNT(EHB=1, EVNTCOUNT=0) -- clears W1C EVNT_HANDLER_BUSY latch
  // 6: DEVIMOD(IMODI=1000) -- reloads interrupt moderation timer
  ASSERT_EQ(ops->size(), 7u);
  EXPECT_EQ((*ops)[0].kind, OpKind::kGevntsizWrite);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[0].value).EVNTINTRPTMASK(), 1u);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[0].value).EVENTSIZ(), kBufferSize);

  EXPECT_EQ((*ops)[1].kind, OpKind::kGevntcountWrite);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[1].value).EVNT_HANDLER_BUSY(), 0u);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[1].value).EVNTCOUNT(), sizeof(uint32_t));

  EXPECT_EQ((*ops)[2].kind, OpKind::kResetEventDispatched);

  EXPECT_EQ((*ops)[3].kind, OpKind::kGevntcountWrite);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[3].value).EVNT_HANDLER_BUSY(), 0u);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[3].value).EVNTCOUNT(), sizeof(uint32_t));

  EXPECT_EQ((*ops)[4].kind, OpKind::kGevntsizWrite);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[4].value).EVNTINTRPTMASK(), 0u);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[4].value).EVENTSIZ(), kBufferSize);

  EXPECT_EQ((*ops)[5].kind, OpKind::kGevntcountWrite);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[5].value).EVNT_HANDLER_BUSY(), 1u);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[5].value).EVNTCOUNT(), 0u);

  EXPECT_EQ((*ops)[6].kind, OpKind::kDevimodWrite);
  EXPECT_EQ(DEVIMOD::Get(0).FromValue((*ops)[6].value).IMODI(), 1000u);

  // When interrupt moderation is disabled (0), HandleIrq must not write EVNT_HANDLER_BUSY=1 or
  // touch DEVIMOD(0).
  ops->clear();
  pending_event_bytes->store(sizeof(uint32_t));
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetInterruptModeration(drv, 0);
    ASSERT_OK(Dwc3TestHelper::PushEventToFifo(drv, MakeDeviceEvent(DEVT_SOF)));
    Dwc3TestHelper::HandleIrq(drv);
  });

  ASSERT_EQ(ops->size(), 3u);
  EXPECT_EQ((*ops)[0].kind, OpKind::kGevntsizWrite);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[0].value).EVNTINTRPTMASK(), 1u);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[0].value).EVENTSIZ(), kBufferSize);
  EXPECT_EQ((*ops)[1].kind, OpKind::kGevntcountWrite);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[1].value).EVNT_HANDLER_BUSY(), 0u);
  EXPECT_EQ(GEVNTCOUNT::Get(0).FromValue((*ops)[1].value).EVNTCOUNT(), sizeof(uint32_t));
  EXPECT_EQ((*ops)[2].kind, OpKind::kGevntsizWrite);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[2].value).EVNTINTRPTMASK(), 0u);
  EXPECT_EQ(GEVNTSIZ::Get(0).FromValue((*ops)[2].value).EVENTSIZ(), kBufferSize);

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, StartEventsKeepsInterruptsMaskedUntilConfiguredAndEnablesErrorEvents) {
  SetUpAndPowerOnDriver();

  enum class InitReg : uint8_t { kGevntsiz, kGevntadr, kGevntcount, kDevten };
  struct InitWrite {
    InitReg reg;
    uint64_t value;
  };
  auto writes = std::make_shared<std::vector<InitWrite>>();

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTADR::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[DEVTEN::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([writes](Environment& env) {
    env.reg_region()[GEVNTSIZ::Get(0).addr()].SetWriteCallback(
        [writes](uint64_t val) { writes->push_back({InitReg::kGevntsiz, val}); });
    env.reg_region()[GEVNTADR::Get(0).addr()].SetWriteCallback(
        [writes](uint64_t val) { writes->push_back({InitReg::kGevntadr, val}); });
    env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetWriteCallback(
        [writes](uint64_t val) { writes->push_back({InitReg::kGevntcount, val}); });
    env.reg_region()[DEVTEN::Get().addr()].SetWriteCallback(
        [writes](uint64_t val) { writes->push_back({InitReg::kDevten, val}); });
  });

  for (uint32_t imodi : {0u, 1000u}) {
    writes->clear();
    dut_.RunInDriverContext([&](Dwc3& drv) {
      Dwc3TestHelper::SetInterruptModeration(drv, imodi);
      Dwc3TestHelper::StartEvents(drv);
    });

    ASSERT_GE(writes->size(), 5u);
    EXPECT_EQ((*writes)[0].reg, InitReg::kGevntsiz);
    const auto initial_siz = GEVNTSIZ::Get(0).FromValue(static_cast<uint32_t>((*writes)[0].value));
    EXPECT_EQ(initial_siz.EVNTINTRPTMASK(), 1u);
    EXPECT_EQ(initial_siz.EVENTSIZ(), kBufferSize);

    const auto& final_write = writes->back();
    EXPECT_EQ(final_write.reg, InitReg::kGevntsiz);
    const auto final_siz = GEVNTSIZ::Get(0).FromValue(static_cast<uint32_t>(final_write.value));
    EXPECT_EQ(final_siz.EVNTINTRPTMASK(), 0u);
    EXPECT_EQ(final_siz.EVENTSIZ(), kBufferSize);

    bool saw_adr = false;
    bool saw_count = false;
    bool saw_devten = false;
    for (size_t i = 1; i + 1 < writes->size(); ++i) {
      switch ((*writes)[i].reg) {
        case InitReg::kGevntadr:
          saw_adr = true;
          break;
        case InitReg::kGevntcount: {
          saw_count = true;
          const auto cnt = GEVNTCOUNT::Get(0).FromValue(static_cast<uint32_t>((*writes)[i].value));
          EXPECT_EQ(cnt.EVNTCOUNT(), 0u);
          EXPECT_EQ(cnt.EVNT_HANDLER_BUSY(), imodi != 0 ? 1u : 0u);
          break;
        }
        case InitReg::kDevten: {
          saw_devten = true;
          const auto devten = DEVTEN::Get().FromValue(static_cast<uint32_t>((*writes)[i].value));
          EXPECT_EQ(devten.EVNTOVERFLOWEN(), 1u);
          EXPECT_EQ(devten.ERRTICERREVTEN(), 1u);
          EXPECT_EQ(devten.CONNECTDONEEVTEN(), 1u);
          EXPECT_EQ(devten.USBRSTEVTEN(), 1u);
          EXPECT_EQ(devten.DISSCONNEVTEN(), 1u);
          break;
        }
        case InitReg::kGevntsiz:
          ADD_FAILURE() << "Unexpected intermediate GEVNTSIZ write before unmask";
          break;
      }
    }
    EXPECT_TRUE(saw_adr);
    EXPECT_TRUE(saw_count);
    EXPECT_TRUE(saw_devten);
  }

  TearDownAndPowerOffDriver();
}

TEST_F(Dwc3EventsTest, HandleOverflowAndErraticErrorEventsRecordMetrics) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleEvent(drv, MakeDeviceEvent(DEVT_EVENT_BUF_OVERFLOW));
    Dwc3TestHelper::HandleEvent(drv, MakeDeviceEvent(DEVT_ERRATIC_ERROR));

    inspect::Inspector inspector = Dwc3TestHelper::RecordMetrics(drv);
    auto hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo()).take_value();
    const auto* history = hierarchy.GetByPath({"event_history"});
    ASSERT_NE(history, nullptr);

    bool saw_overflow = false;
    bool saw_erratic = false;
    for (const auto& child : history->children()) {
      const auto* msg = child.node().get_property<inspect::StringPropertyValue>("event");
      if (msg != nullptr) {
        if (msg->value() == "Event Buffer Overflow") {
          saw_overflow = true;
        } else if (msg->value() == "USB Erratic Error") {
          saw_erratic = true;
        }
      }
    }
    EXPECT_TRUE(saw_overflow);
    EXPECT_TRUE(saw_erratic);
  });

  TearDownAndPowerOffDriver();
}

}  // namespace dwc3
