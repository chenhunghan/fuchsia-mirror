// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc2/dwc2.h"

#include <fidl/fuchsia.driver.compat/cpp/test_base.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fdf/dispatcher.h>
#include <lib/fit/defer.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/result.h>
#include <lib/zx/vmo.h>

#include <atomic>
#include <mutex>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc2/dwc2_config.h"
#include "src/devices/usb/drivers/dwc2/usb_dwc_regs.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace dwc2 {

namespace fpdev = fuchsia_hardware_platform_device;

class Environment : public fdf_testing::Environment {
 public:
  Environment() {
    fdf_fake::FakePDev::Config cfg{};
    cfg.mmios[0] = mmio_.GetMmioBuffer();
    cfg.use_fake_bti = true;
    cfg.use_fake_irq = true;

    // Pretend to be a v3.30a DWC2 core; the same version used in AML smart
    // display devices.
    //
    // All these tests care about is init/shutdown, not actually starting or
    // operating the controller.  All we should need to do is emulate:
    // 1) the SynopsisID register
    // 2) HWCFG2 (which reports whether or not dynamic FIFOs are supported)
    // 3) HWCFG4 (which reports whether or not IN endpoints use a shared FIFO or dedicated FIFOs)
    // 4) and the global reset register (GRSTCTL).
    mmio_[GSNPSID::Get().addr()].SetReadCallback([]() -> uint64_t { return 0x4f54330a; });
    mmio_[GHWCFG2::Get().addr()].SetReadCallback([]() -> uint64_t { return 0x2288d854; });
    mmio_[GHWCFG3::Get().addr()].SetReadCallback([]() -> uint64_t { return 0x40000000; });
    mmio_[GHWCFG4::Get().addr()].SetReadCallback([]() -> uint64_t { return 0xd6028030; });
    mmio_[GRSTCTL::Get().addr()].SetReadCallback([this]() { return fake_grstctl_.Read(); });
    mmio_[GRSTCTL::Get().addr()].SetWriteCallback([this](uint64_t v) { fake_grstctl_.Write(v); });

    pdev_.SetConfig(std::move(cfg));
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    static const fuchsia_hardware_usb_dwc2::Metadata kMetadata(
        {.dma_burst_len = fuchsia_hardware_usb_dwc2::DmaBurstLen::kSingle,
         .usb_turnaround_time = 10,
         .rx_fifo_size = 1024,
         .nptx_fifo_size = 1024,
         .tx_fifo_sizes =
             std::array<uint32_t, 15>{1024, 1024, 1024, 1024, 1024, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0}});

    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    pdev_.AddFidlMetadata(fuchsia_hardware_usb_dwc2::Metadata::kSerializableName, kMetadata);
    EXPECT_EQ(to_driver_vfs.AddService<fpdev::Service>(pdev_.GetInstanceHandler(dispatcher), "pdev")
                  .status_value(),
              ZX_OK);

    return zx::ok();
  }

  fake_mmio::FakeMmioRegRegion& mmio() { return mmio_; }

  void ResetGrstctlCallbacks() {
    fake_grstctl_.Reset();
    mmio_[GRSTCTL::Get().addr()].SetReadCallback([this]() { return fake_grstctl_.Read(); });
    mmio_[GRSTCTL::Get().addr()].SetWriteCallback([this](uint64_t v) { fake_grstctl_.Write(v); });
  }

 private:
  // A class which manages a poor emulation of the global reset register (for
  // core silicon versions < 4.20a).  The only thing this emulation does is look
  // for writes to the Soft Reset bit, after which it will pretend to be
  // resetting for 3 read-cycles, and report that the AHB is busy for 6
  // read-cycles.
  class FakeGRSTCTL {
   public:
    FakeGRSTCTL() { Reset(); }

    void Reset() {
      val_ = GRSTCTL::Get().FromValue(0).set_ahbidle(1).reg_value();
      reset_in_progress_cycles_ = 0;
    }

    uint64_t Read() {
      GRSTCTL new_val = GRSTCTL::Get().FromValue(val_);

      if (reset_in_progress_cycles_ <= kSoftResetBitCycleThresh) {
        new_val.set_csftrst(0);
      }

      if (reset_in_progress_cycles_ <= kAHBIdleCycleThresh) {
        new_val.set_ahbidle(1);
      }

      if (reset_in_progress_cycles_) {
        --reset_in_progress_cycles_;
      }

      return val_ = new_val.reg_value();
    }

    void Write(uint64_t val64) {
      // We don't expect the DUT to be doing anything but reading the existing
      // value and attempting to set the csftrst bit.  If it tries to set any
      // other bit, fail the test so that someone can come back here and update
      // the test.
      ASSERT_EQ(uint64_t{0}, val64 & 0xFFFF'FFFF'0000'0000);
      const uint32_t val = static_cast<uint32_t>(val64);
      const uint32_t disallow_mask = ~GRSTCTL::Get()
                                          .FromValue(0)
                                          .set_csftrst(1)
                                          .set_ahbidle(1)
                                          .set_txfflsh(1)
                                          .set_rxfflsh(1)
                                          .set_txfnum(0x1F)
                                          .reg_value();
      EXPECT_EQ(0u, val & disallow_mask);

      // If the user is setting the Soft Reset bit, set the bit in our state as
      // well as clearing the AHBIdle state, and set the reset-cycle countdown
      // so that these bits will clear over time as the register is read.
      GRSTCTL set_val = GRSTCTL::Get().FromValue(val);
      if (set_val.csftrst()) {
        val_ = GRSTCTL::Get().FromValue(val_).set_csftrst(1).set_ahbidle(0).reg_value();
        reset_in_progress_cycles_ = kTotalResetCycles;
      }
    }

   private:
    uint32_t kTotalResetCycles = 6;
    uint32_t kSoftResetBitCycleThresh = 3;
    uint32_t kAHBIdleCycleThresh = 0;

    uint32_t val_;
    uint32_t reset_in_progress_cycles_;
  };

  fdf_fake::FakePDev pdev_;
  fake_mmio::FakeMmioRegRegion mmio_{sizeof(uint32_t), 4096};
  FakeGRSTCTL fake_grstctl_;
};

class Config {
 public:
  using DriverType = Dwc2;
  using EnvironmentType = Environment;
};

class Dwc2Test : public testing::Test {
 public:
  void SetUp() override {
    ASSERT_TRUE(dut_.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
                      dwc2_config::Config cfg;
                      cfg.enable_suspend() = false;
                      args.config(cfg.ToVmo());
                    })
                    .is_ok());
  }

  void TearDown() override { EXPECT_TRUE(dut_.StopDriver().is_ok()); }

 protected:
  Dwc2::Endpoint* GetEndpoint(Dwc2& driver, uint8_t ep_num) {
    if (ep_num < std::size(driver.endpoints_) && driver.endpoints_[ep_num].has_value()) {
      return &*driver.endpoints_[ep_num];
    }
    return nullptr;
  }

  void QueueNextRequest(Dwc2& driver, Dwc2::Endpoint* ep) __TA_REQUIRES(ep->lock) {
    driver.QueueNextRequest(ep);
  }

  void SetRemainingTransferSize(uint8_t ep_num, uint32_t xfersize) {
    dut_.RunInEnvironmentTypeContext([ep_num, xfersize](Environment& env) {
      env.mmio()[DEPTSIZ::Get(ep_num).addr()].SetReadCallback([ep_num, xfersize]() -> uint64_t {
        return DEPTSIZ::Get(ep_num).FromValue(0).set_xfersize(xfersize).reg_value();
      });
    });
  }

  void SetReqXferSize(uint8_t ep_num, uint32_t req_xfersize) {
    dut_.RunInDriverContext([ep_num, req_xfersize](Dwc2& driver) {
      ASSERT_TRUE(driver.endpoints_[ep_num].has_value());
      driver.endpoints_[ep_num]->req_xfersize = req_xfersize;
    });
  }

  uint32_t ReadTransferred(uint8_t ep_num) {
    uint32_t transferred = 0;
    dut_.RunInDriverContext([ep_num, &transferred](Dwc2& driver) {
      ZX_ASSERT(driver.endpoints_[ep_num].has_value());
      transferred = driver.ReadTransferred(&*driver.endpoints_[ep_num]);
    });
    return transferred;
  }

  void SetConnectedState(Dwc2& driver, bool connected) { driver.connected_.store(connected); }

  void SetConfiguredState(Dwc2& driver, bool configured) { driver.configured_.store(configured); }

  void SetConnected(Dwc2& driver, bool connected) { driver.SetConnected(connected); }

  bool IsConnected(const Dwc2& driver) const { return driver.connected_.load(); }

  bool IsConfigured(const Dwc2& driver) const { return driver.configured_.load(); }

  size_t GetQuarantinedRequestCount(Dwc2& driver) {
    std::lock_guard<std::mutex> lock(driver.lock_);
    return driver.quarantined_reqs_.size();
  }

  uint64_t GetBtiPmoCount(const Dwc2& driver) const {
    zx_info_bti_t info{};
    EXPECT_EQ(driver.bti().get_info(ZX_INFO_BTI, &info, sizeof(info), nullptr, nullptr), ZX_OK);
    return info.pmo_count;
  }

  // Inspects whether Dwc2.lock_ is locked.
  // This check is sound here because the driver dispatcher thread is synchronously blocked
  // awaiting the SetConnected FIDL response from FakeUsbDciInterface.
  bool IsDriverLocked(Dwc2& driver) const __TA_NO_THREAD_SAFETY_ANALYSIS {
    if (driver.lock_.try_lock()) {
      driver.lock_.unlock();
      return false;
    }
    return true;
  }

  void HandleEp0TimeoutRecovery(Dwc2& driver) { driver.HandleEp0TimeoutRecovery(); }

  fidl::SyncClient<fuchsia_hardware_usb_dci::UsbDci> ConnectDciClient() {
    zx::result dci_client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
    EXPECT_TRUE(dci_client_end.is_ok());
    if (dci_client_end.is_error()) {
      return {};
    }
    return fidl::SyncClient<fuchsia_hardware_usb_dci::UsbDci>(std::move(*dci_client_end));
  }

  void ConfigureBulkEndpoint(fidl::SyncClient<fuchsia_hardware_usb_dci::UsbDci>& dci_client,
                             uint8_t ep_addr, uint16_t max_packet_size = 64) {
    fdescriptor::UsbEndpointDescriptor ep_desc{};
    ep_desc.b_endpoint_address(ep_addr);
    ep_desc.bm_attributes(static_cast<uint8_t>(fdescriptor::EndpointType::kBulk));
    ep_desc.w_max_packet_size(max_packet_size);
    fdescriptor::UsbSsEpCompDescriptor ss_comp_desc{};

    auto config_result = dci_client->ConfigureEndpoint({ep_desc, ss_comp_desc});
    ASSERT_TRUE(config_result.is_ok()) << config_result.error_value().FormatDescription();
  }

  fdf_testing::BackgroundDriverTest<Config> dut_;
};

TEST_F(Dwc2Test, DriverLifetimeTest) {}

TEST_F(Dwc2Test, GetHardwareInfo) {
  zx::result client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::SyncClient client(std::move(*client_end));

  auto result = client->GetHardwareInfo();
  ASSERT_TRUE(result.is_ok()) << result.error_value().FormatDescription();

  auto& info = result->info();
  // The number of endpoints (5 IN + 5 OUT) is derived from the fake GHWCFG2
  // and GHWCFG4 values set in the Environment.
  // 5 IN endpoints (capped by num_dev_ep = 6) + 5 OUT endpoints (num_dev_ep = 6, minus EP0)
  EXPECT_EQ(info.endpoints()->size(), 10u);

  // EP1 IN:
  EXPECT_EQ(info.endpoints()->at(0).ep_address(), 0x81);
  // The max_packet_size_limit for IN endpoints is 4096 because the emulated
  // GHWCFG4 (0xd6028030) indicates support for dedicated IN FIFOs, and the
  // driver combines 4x 1024-byte TX FIFOs (from metadata) for IN endpoints.
  ASSERT_TRUE(info.endpoints()->at(0).supported_types().has_value());
  EXPECT_EQ(info.endpoints()->at(0).supported_types()->size(), 2u);
  EXPECT_EQ(info.endpoints()->at(0).supported_types()->at(0).max_packet_size_limit(), 4096u);
  EXPECT_EQ(info.endpoints()->at(0).supported_types()->at(0).endpoint_type(),
            fdescriptor::EndpointType::kBulk);
  EXPECT_EQ(info.endpoints()->at(0).supported_types()->at(1).max_packet_size_limit(), 4096u);
  EXPECT_EQ(info.endpoints()->at(0).supported_types()->at(1).endpoint_type(),
            fdescriptor::EndpointType::kInterrupt);

  // EP1 OUT (after 5 IN endpoints):
  EXPECT_EQ(info.endpoints()->at(5).ep_address(), 0x01);
  // The max_packet_size_limit for OUT endpoints is 1024, matching the
  // rx_fifo_size provided in the metadata.
  ASSERT_TRUE(info.endpoints()->at(5).supported_types().has_value());
  EXPECT_EQ(info.endpoints()->at(5).supported_types()->size(), 2u);
  EXPECT_EQ(info.endpoints()->at(5).supported_types()->at(0).max_packet_size_limit(), 1024u);
  EXPECT_EQ(info.endpoints()->at(5).supported_types()->at(0).endpoint_type(),
            fdescriptor::EndpointType::kBulk);
  EXPECT_EQ(info.endpoints()->at(5).supported_types()->at(1).max_packet_size_limit(), 1024u);
  EXPECT_EQ(info.endpoints()->at(5).supported_types()->at(1).endpoint_type(),
            fdescriptor::EndpointType::kInterrupt);
}

TEST_F(Dwc2Test, ReadTransferred) {
  constexpr uint8_t kEpNum = 1;
  constexpr uint32_t kReqXferSize = 100;

  ASSERT_NO_FATAL_FAILURE(SetReqXferSize(kEpNum, kReqXferSize));

  // Case 1: remaining > ep->req_xfersize.
  // Hardware reports more remaining bytes (150) than programmed (100).
  // ReadTransferred logs a warning, clamps remaining to req_xfersize to avoid underflow, and
  // returns 0.
  SetRemainingTransferSize(kEpNum, 150);
  EXPECT_EQ(ReadTransferred(kEpNum), 0u);

  // Case 2: remaining == ep->req_xfersize.
  // 100 remaining out of 100 programmed bytes means 0 bytes transferred.
  SetRemainingTransferSize(kEpNum, kReqXferSize);
  EXPECT_EQ(ReadTransferred(kEpNum), 0u);

  // Case 3: remaining < ep->req_xfersize.
  // 40 remaining out of 100 programmed bytes means 60 bytes transferred.
  SetRemainingTransferSize(kEpNum, 40);
  EXPECT_EQ(ReadTransferred(kEpNum), 60u);

  // Case 4: remaining == 0.
  // 0 remaining out of 100 programmed bytes means 100 bytes transferred.
  SetRemainingTransferSize(kEpNum, 0);
  EXPECT_EQ(ReadTransferred(kEpNum), kReqXferSize);
}

TEST_F(Dwc2Test, PendingZlp_SetOnInShortTransfer) {
  dut_.RunInDriverContext([this](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(0x81);  // EP 1 IN
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(ep, nullptr);

    std::lock_guard<std::mutex> lock(ep->lock);
    ep->max_packet_size = 64;
    ep->enabled = true;
    EXPECT_FALSE(ep->pending_zlp);

    usb::FidlRequest fidl_req(usb::EndpointType::BULK);
    fidl_req->short_(true);
    fidl_req.add_data(std::vector<uint8_t>(64, 0), 64, 0);

    ep->queued_reqs.push(std::move(fidl_req));

    QueueNextRequest(driver, ep);
    EXPECT_TRUE(ep->pending_zlp);
  });
}

TEST_F(Dwc2Test, PendingZlp_NotSetWithoutShortFlag) {
  dut_.RunInDriverContext([this](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(0x81);  // EP 1 IN
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(ep, nullptr);

    std::lock_guard<std::mutex> lock(ep->lock);
    ep->max_packet_size = 64;
    ep->enabled = true;
    EXPECT_FALSE(ep->pending_zlp);

    usb::FidlRequest fidl_req(usb::EndpointType::BULK);
    fidl_req->short_(false);
    fidl_req.add_data(std::vector<uint8_t>(64, 0), 64, 0);

    ep->queued_reqs.push(std::move(fidl_req));

    QueueNextRequest(driver, ep);
    EXPECT_FALSE(ep->pending_zlp);
  });
}

TEST_F(Dwc2Test, PendingZlp_NotSetIfLengthNotMultiple) {
  dut_.RunInDriverContext([this](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(0x81);  // EP 1 IN
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(ep, nullptr);

    std::lock_guard<std::mutex> lock(ep->lock);
    ep->max_packet_size = 64;
    ep->enabled = true;
    EXPECT_FALSE(ep->pending_zlp);

    usb::FidlRequest fidl_req(usb::EndpointType::BULK);
    fidl_req->short_(true);
    fidl_req.add_data(std::vector<uint8_t>(50, 0), 50, 0);

    ep->queued_reqs.push(std::move(fidl_req));

    QueueNextRequest(driver, ep);
    EXPECT_FALSE(ep->pending_zlp);
  });
}

TEST_F(Dwc2Test, PendingZlp_ClearedOnCancelAll) {
  dut_.RunInDriverContext([this](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(0x81);  // EP 1 IN
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(ep, nullptr);

    {
      std::lock_guard<std::mutex> lock(ep->lock);
      ep->max_packet_size = 64;
      ep->enabled = true;
      ASSERT_FALSE(ep->pending_zlp);

      usb::FidlRequest fidl_req(usb::EndpointType::BULK);
      fidl_req->short_(true);
      fidl_req.add_data(std::vector<uint8_t>(64, 0), 64, 0);

      ep->queued_reqs.push(std::move(fidl_req));

      QueueNextRequest(driver, ep);
      ASSERT_TRUE(ep->pending_zlp);
    }

    // CancelAll() acquires lock.
    ep->CancelAll();
    EXPECT_FALSE(ep->pending_zlp);
  });
}

namespace {

class FakeUsbDciInterface : public fidl::WireServer<fuchsia_hardware_usb_dci::UsbDciInterface> {
 public:
  using SetConnectedCallback = fit::function<void(bool connected)>;

  void set_connected_callback(SetConnectedCallback cb) { set_connected_cb_ = std::move(cb); }

  void SetConnected(SetConnectedRequestView request,
                    SetConnectedCompleter::Sync& completer) override {
    if (set_connected_cb_) {
      set_connected_cb_(request->is_connected);
    }
    completer.Reply(zx::ok());
  }

  void SetSpeed(SetSpeedRequestView request, SetSpeedCompleter::Sync& completer) override {
    completer.Reply(zx::ok());
  }

  void Control(ControlRequestView request, ControlCompleter::Sync& completer) override {
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_dci::UsbDciInterface> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  SetConnectedCallback set_connected_cb_;
};

}  // namespace

TEST_F(Dwc2Test, Ep0TimeoutRecovery_DoesNotDeadlockWithCancelAll) {
  FakeUsbDciInterface fake_dci;
  fdf::UnownedSynchronizedDispatcher dci_dispatcher = dut_.runtime().StartBackgroundDispatcher();

  auto cleanup_dispatcher = fit::defer([&] {
    libsync::Completion shutdown_complete;
    dut_.runtime().ShutdownBackgroundDispatcher(dci_dispatcher->get(),
                                                [&] { shutdown_complete.Signal(); });
    shutdown_complete.Wait();
  });

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(endpoints.is_ok());

  auto binding =
      fidl::BindServer(dci_dispatcher->async_dispatcher(), std::move(endpoints->server), &fake_dci);

  zx::result dci_client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_client_end.is_ok());
  fidl::SyncClient dci_client(std::move(*dci_client_end));

  auto set_intf_result = dci_client->SetInterface({std::move(endpoints->client)});
  ASSERT_TRUE(set_intf_result.is_ok()) << set_intf_result.error_value().FormatDescription();

  dut_.RunInDriverContext([&](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(0x81);  // EP 1 IN
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(nullptr, ep);

    {
      std::lock_guard<std::mutex> lock(ep->lock);
      ep->max_packet_size = 64;
      ep->enabled = true;

      usb::FidlRequest fidl_req(usb::EndpointType::BULK);
      fidl_req->short_(true);
      fidl_req.add_data(std::vector<uint8_t>(64, 0), 64, 0);

      ep->queued_reqs.push(std::move(fidl_req));
      QueueNextRequest(driver, ep);
      ASSERT_TRUE(ep->pending_zlp);
    }

    SetConnectedState(driver, true);

    std::atomic<bool> driver_was_locked = true;
    std::atomic<bool> cancel_completed = false;
    fake_dci.set_connected_callback(
        [&driver, ep, &driver_was_locked, &cancel_completed, this](bool connected) {
          if (!connected) {
            // Verify that driver.lock_ is NOT held during SetConnected(false).
            // The lock hierarchy in dwc2.h specifies that endpoint locks must be acquired
            // before Dwc2.lock_, so Dwc2.lock_ must never be held when notifying the DCI
            // interface of a disconnect.
            driver_was_locked = IsDriverLocked(driver);
            if (!driver_was_locked) {
              // Simulate usb-cdc-function cancelling endpoints upon receiving disconnect from
              // usb-peripheral. In the buggy code, CancelAll() attempts to acquire driver.lock_
              // while holding ep->lock, deadlocking with HandleEp0TimeoutRecovery() which held
              // driver.lock_ and tried to acquire ep->lock.
              ep->CancelAll();
              cancel_completed = true;
            }
          }
        });

    // Trigger EP0 timeout recovery.
    HandleEp0TimeoutRecovery(driver);

    EXPECT_FALSE(driver_was_locked);
    EXPECT_TRUE(cancel_completed);
    EXPECT_FALSE(ep->pending_zlp);
    EXPECT_FALSE(IsConnected(driver));
  });
}

namespace {

class EndpointEventHandler
    : public fidl::AsyncEventHandler<fuchsia_hardware_usb_endpoint::Endpoint> {
 public:
  struct CompletionRecord {
    zx_status_t status;
    std::optional<bool> is_short;
    std::optional<size_t> length;
  };

  void OnCompletion(
      fidl::Event<fuchsia_hardware_usb_endpoint::Endpoint::OnCompletion>& event) override {
    std::lock_guard<std::mutex> lock(mutex_);
    auto signal = fit::defer([this] { completion_.Signal(); });
    ASSERT_FALSE(event.completion().empty());
    for (const auto& completion : event.completion()) {
      ASSERT_TRUE(completion.status().has_value());
      std::optional<bool> is_short;
      std::optional<size_t> length;
      if (completion.request().has_value()) {
        is_short = completion.request()->short_();
        if (completion.request()->data().has_value()) {
          size_t total = 0;
          for (const auto& region : *completion.request()->data()) {
            total += region.size().value_or(0);
          }
          length = total;
        }
      }
      completions_.push({
          .status = *completion.status(),
          .is_short = is_short,
          .length = length,
      });
    }
  }

  zx::result<CompletionRecord> WaitForCompletion(zx::duration timeout) {
    const zx::time deadline = zx::deadline_after(timeout);
    while (true) {
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (!completions_.empty()) {
          CompletionRecord record = completions_.front();
          completions_.pop();
          if (completions_.empty()) {
            completion_.Reset();
          }
          return zx::ok(record);
        }
        completion_.Reset();
      }
      zx_status_t wait_status = completion_.Wait(deadline);
      if (wait_status != ZX_OK) {
        return zx::error(wait_status);
      }
    }
  }

  CompletionRecord WaitForCompletion() {
    zx::result<CompletionRecord> record = WaitForCompletion(zx::duration::infinite());
    EXPECT_TRUE(record.is_ok()) << record.status_string();
    return record.value_or(
        CompletionRecord{.status = record.status_value(), .is_short = {}, .length = {}});
  }

  zx_status_t WaitForStatus() { return WaitForCompletion().status; }

  size_t PendingCompletionCount() {
    std::lock_guard<std::mutex> lock(mutex_);
    return completions_.size();
  }

 private:
  std::mutex mutex_;
  std::queue<CompletionRecord> completions_ __TA_GUARDED(mutex_);
  libsync::Completion completion_;
};

}  // namespace

TEST_F(Dwc2Test, SetConnectedFalseDrainsEndpointsBeforeNotifyingDci) {
  FakeUsbDciInterface fake_dci;
  fdf::UnownedSynchronizedDispatcher dci_dispatcher = dut_.runtime().StartBackgroundDispatcher();
  fdf::UnownedSynchronizedDispatcher ep_dispatcher = dut_.runtime().StartBackgroundDispatcher();

  auto cleanup_dispatchers = fit::defer([&] {
    libsync::Completion dci_shutdown;
    dut_.runtime().ShutdownBackgroundDispatcher(dci_dispatcher->get(),
                                                [&] { dci_shutdown.Signal(); });
    dci_shutdown.Wait();

    libsync::Completion ep_shutdown;
    dut_.runtime().ShutdownBackgroundDispatcher(ep_dispatcher->get(),
                                                [&] { ep_shutdown.Signal(); });
    ep_shutdown.Wait();
  });

  auto dci_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_dci::UsbDciInterface>();
  ASSERT_TRUE(dci_endpoints.is_ok());
  auto binding = fidl::BindServer(dci_dispatcher->async_dispatcher(),
                                  std::move(dci_endpoints->server), &fake_dci);

  zx::result dci_client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_client_end.is_ok());
  fidl::SyncClient dci_client(std::move(*dci_client_end));

  auto set_intf_result = dci_client->SetInterface({std::move(dci_endpoints->client)});
  ASSERT_TRUE(set_intf_result.is_ok()) << set_intf_result.error_value().FormatDescription();

  auto ep_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(ep_endpoints.is_ok());

  constexpr uint8_t kEpAddr = 0x81;  // EP 1 IN
  auto connect_result = dci_client->ConnectToEndpoint({kEpAddr, std::move(ep_endpoints->server)});
  ASSERT_TRUE(connect_result.is_ok());

  EndpointEventHandler event_handler;
  fidl::SharedClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(
      std::move(ep_endpoints->client), ep_dispatcher->async_dispatcher(), &event_handler);

  bool callback_invoked = false;
  bool configured_in_cb = true;
  bool ep_enabled_in_cb = true;
  bool pending_zlp_in_cb = true;
  bool has_current_req_in_cb = true;
  bool queued_reqs_empty_in_cb = false;

  dut_.RunInDriverContext([&](Dwc2& driver) {
    auto* ep0_in = GetEndpoint(driver, DWC_EP0_IN);
    auto* ep0_out = GetEndpoint(driver, DWC_EP0_OUT);
    ASSERT_NE(ep0_in, nullptr);
    ASSERT_NE(ep0_out, nullptr);
    ep0_in->req_offset = 8;
    ep0_in->req_xfersize = 32;
    ep0_in->req_length = 64;
    ep0_out->req_offset = 16;
    ep0_out->req_xfersize = 64;
    ep0_out->req_length = 128;

    uint8_t ep_num = DWC_ADDR_TO_INDEX(kEpAddr);
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(nullptr, ep);

    {
      std::lock_guard<std::mutex> lock(ep->lock);
      ep->max_packet_size = 64;
      ep->enabled = true;
      ep->pending_zlp = true;

      usb::FidlRequest active_req(usb::EndpointType::BULK);
      active_req->defer_completion(false);
      active_req->short_(true);
      ep->current_req.emplace(std::move(active_req));

      usb::FidlRequest queued_req(usb::EndpointType::BULK);
      queued_req->defer_completion(false);
      queued_req->short_(false);
      ep->queued_reqs.push(std::move(queued_req));
    }

    SetConnectedState(driver, true);
    SetConfiguredState(driver, true);

    fake_dci.set_connected_callback([&](bool connected) {
      if (!connected) {
        callback_invoked = true;
        configured_in_cb = IsConfigured(driver);
        std::lock_guard<std::mutex> lock(ep->lock);
        ep_enabled_in_cb = ep->enabled;
        pending_zlp_in_cb = ep->pending_zlp;
        has_current_req_in_cb = ep->current_req.has_value();
        queued_reqs_empty_in_cb = ep->queued_reqs.empty();
      }
    });

    // dci_intf_->SetConnected() is a synchronous two-way fidl::WireSyncClient call, so
    // set_connected_cb_ runs to completion on dci_dispatcher before SetConnected() and
    // RunInDriverContext() return.
    SetConnected(driver, false);

    // EP0 IN/OUT transfer counters belong to IrqThread without ep->lock and must not be reset by
    // SetConnected(false).
    EXPECT_EQ(ep0_in->req_offset, 8u);
    EXPECT_EQ(ep0_in->req_xfersize, 32u);
    EXPECT_EQ(ep0_in->req_length, 64u);
    EXPECT_EQ(ep0_out->req_offset, 16u);
    EXPECT_EQ(ep0_out->req_xfersize, 64u);
    EXPECT_EQ(ep0_out->req_length, 128u);
  });

  EXPECT_TRUE(callback_invoked);
  EXPECT_FALSE(configured_in_cb);
  EXPECT_FALSE(ep_enabled_in_cb);
  EXPECT_FALSE(pending_zlp_in_cb);
  EXPECT_FALSE(has_current_req_in_cb);
  EXPECT_TRUE(queued_reqs_empty_in_cb);

  // Verify that the in-flight request (current_req, short_ == true) is completed before the
  // queued request (queued_reqs, short_ == false).
  EndpointEventHandler::CompletionRecord first_completion = event_handler.WaitForCompletion();
  EXPECT_EQ(first_completion.status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(first_completion.is_short, std::optional<bool>(true));

  EndpointEventHandler::CompletionRecord second_completion = event_handler.WaitForCompletion();
  EXPECT_EQ(second_completion.status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(second_completion.is_short, std::optional<bool>(false));
}

TEST_F(Dwc2Test, QueueRequestOnDisabledOrUnconfiguredEndpointReturnsIoNotPresent) {
  fdf::UnownedSynchronizedDispatcher ep_dispatcher = dut_.runtime().StartBackgroundDispatcher();
  auto cleanup_dispatcher = fit::defer([&] {
    libsync::Completion shutdown_complete;
    dut_.runtime().ShutdownBackgroundDispatcher(ep_dispatcher->get(),
                                                [&] { shutdown_complete.Signal(); });
    shutdown_complete.Wait();
  });

  zx::result dci_client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(dci_client_end.is_ok());
  fidl::SyncClient dci_client(std::move(*dci_client_end));

  auto ep_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(ep_endpoints.is_ok());

  constexpr uint8_t kEpAddr = 0x81;  // EP 1 IN
  auto connect_result = dci_client->ConnectToEndpoint({kEpAddr, std::move(ep_endpoints->server)});
  ASSERT_TRUE(connect_result.is_ok());

  EndpointEventHandler event_handler;
  fidl::SharedClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(
      std::move(ep_endpoints->client), ep_dispatcher->async_dispatcher(), &event_handler);

  // Case 1: Endpoint is disabled while device is configured.
  dut_.RunInDriverContext([&](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(kEpAddr);
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(nullptr, ep);

    {
      std::lock_guard<std::mutex> lock(ep->lock);
      ep->max_packet_size = 64;
      ep->enabled = false;
    }
    SetConfiguredState(driver, true);

    usb::FidlRequest req(usb::EndpointType::BULK);
    req->defer_completion(false);
    ep->QueueRequest(std::move(req));
  });
  EXPECT_EQ(event_handler.WaitForStatus(), ZX_ERR_IO_NOT_PRESENT);

  // Case 2: Endpoint is enabled while device is not configured.
  dut_.RunInDriverContext([&](Dwc2& driver) {
    uint8_t ep_num = DWC_ADDR_TO_INDEX(kEpAddr);
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(nullptr, ep);

    {
      std::lock_guard<std::mutex> lock(ep->lock);
      ep->max_packet_size = 64;
      ep->enabled = true;
    }
    SetConfiguredState(driver, false);

    usb::FidlRequest req(usb::EndpointType::BULK);
    req->defer_completion(false);
    ep->QueueRequest(std::move(req));
  });
  EXPECT_EQ(event_handler.WaitForStatus(), ZX_ERR_IO_NOT_PRESENT);
}

namespace {

struct ScopedEndpointHaltMmio {
  fdf_testing::BackgroundDriverTest<Config>& dut;
  uint8_t ep_num;
  bool is_in;
  uint32_t epint_addr;

  std::atomic<uint32_t> daintmsk = 0;
  std::atomic<uint32_t> depctl = 0;
  std::atomic<uint32_t> depdma = 0;
  std::atomic<uint32_t> epint = 0;
  std::atomic<uint32_t> dctl = 0;
  std::atomic<uint32_t> gintsts = 0;
  std::atomic<bool> saw_epdis = false;
  std::atomic<bool> saw_sgoutnak = false;
  std::atomic<bool> fail_nakeff = false;
  std::atomic<bool> fail_epdis = false;
  std::atomic<bool> clear_epena_on_first_depctl_read = false;
  std::atomic<uint32_t> reset_daintmsk_on_epdis = UINT32_MAX;

  ScopedEndpointHaltMmio(fdf_testing::BackgroundDriverTest<Config>& test_dut, uint8_t ep)
      : dut(test_dut),
        ep_num(ep),
        is_in(DWC_EP_IS_IN(ep)),
        epint_addr(is_in ? DIEPINT::Get(ep).addr() : DOEPINT::Get(ep).addr()) {
    dut.RunInEnvironmentTypeContext([this](Environment& env) {
      env.mmio()[DAINTMSK::Get().addr()].SetReadCallback(
          [this]() -> uint64_t { return daintmsk.load(); });
      env.mmio()[DAINTMSK::Get().addr()].SetWriteCallback(
          [this](uint64_t val) { daintmsk.store(static_cast<uint32_t>(val)); });
      env.mmio()[DEPCTL::Get(ep_num).addr()].SetReadCallback([this]() -> uint64_t {
        const uint32_t cur = depctl.load();
        if (clear_epena_on_first_depctl_read.exchange(false)) {
          depctl.store(DEPCTL::Get(ep_num).FromValue(cur).set_epena(0).reg_value());
        }
        return cur;
      });
      env.mmio()[DEPCTL::Get(ep_num).addr()].SetWriteCallback([this](uint64_t val) {
        const bool was_epena = DEPCTL::Get(ep_num).FromValue(depctl.load()).epena() != 0;
        auto reg = DEPCTL::Get(ep_num).FromValue(static_cast<uint32_t>(val));
        const bool wrote_epena = reg.epena() != 0;
        // DEPCTL.epena is R_WS_SC in hardware: writing 0 does not clear an active transfer.
        if (was_epena) {
          reg.set_epena(1);
        }
        if (is_in && reg.snak() && !fail_nakeff.load()) {
          epint.fetch_or(DIEPINT::Get(ep_num).FromValue(0).set_inepnakeff(1).reg_value());
        }
        // DWC2 requires both EPDis and EPEna to be written as 1 to disable an active endpoint.
        if (reg.epdis() && was_epena && wrote_epena) {
          saw_epdis.store(true);
          const uint32_t override_daintmsk = reset_daintmsk_on_epdis.load();
          if (override_daintmsk != UINT32_MAX) {
            daintmsk.store(override_daintmsk);
          }
          if (!fail_epdis.load()) {
            reg.set_epdis(0).set_epena(0);
            epint.fetch_or(is_in ? DIEPINT::Get(ep_num).FromValue(0).set_epdisabled(1).reg_value()
                                 : DOEPINT::Get(ep_num).FromValue(0).set_epdisabled(1).reg_value());
          } else {
            reg.set_epdis(0);
          }
        } else if (reg.epdis()) {
          reg.set_epdis(0);
        }
        depctl.store(reg.reg_value());
      });
      env.mmio()[DEPDMA::Get(ep_num).addr()].SetReadCallback(
          [this]() -> uint64_t { return depdma.load(); });
      env.mmio()[DEPDMA::Get(ep_num).addr()].SetWriteCallback(
          [this](uint64_t val) { depdma.store(static_cast<uint32_t>(val)); });
      env.mmio()[epint_addr].SetReadCallback([this]() -> uint64_t { return epint.load(); });
      env.mmio()[epint_addr].SetWriteCallback(
          [this](uint64_t val) { epint.fetch_and(~static_cast<uint32_t>(val)); });
      env.mmio()[DCTL::Get().addr()].SetReadCallback([this]() -> uint64_t { return dctl.load(); });
      env.mmio()[DCTL::Get().addr()].SetWriteCallback([this](uint64_t val) {
        auto reg = DCTL::Get().FromValue(static_cast<uint32_t>(val));
        if (reg.sgoutnak()) {
          saw_sgoutnak.store(true);
          reg.set_sgoutnak(0);
          if (!fail_nakeff.load()) {
            reg.set_goutnaksts(1);
            gintsts.fetch_or(GINTSTS::Get().FromValue(0).set_goutnakeff(1).reg_value());
          }
        }
        if (reg.cgoutnak()) {
          reg.set_cgoutnak(0).set_goutnaksts(0);
          gintsts.fetch_and(~GINTSTS::Get().FromValue(0).set_goutnakeff(1).reg_value());
        }
        dctl.store(reg.reg_value());
      });
      env.mmio()[GINTSTS::Get().addr()].SetReadCallback(
          [this]() -> uint64_t { return gintsts.load(); });
    });
  }

  ~ScopedEndpointHaltMmio() {
    dut.RunInEnvironmentTypeContext([this](Environment& env) {
      env.mmio()[DAINTMSK::Get().addr()] = fake_mmio::FakeMmioReg{};
      env.mmio()[DEPCTL::Get(ep_num).addr()] = fake_mmio::FakeMmioReg{};
      env.mmio()[DEPDMA::Get(ep_num).addr()] = fake_mmio::FakeMmioReg{};
      env.mmio()[epint_addr] = fake_mmio::FakeMmioReg{};
      env.mmio()[DCTL::Get().addr()] = fake_mmio::FakeMmioReg{};
      env.mmio()[GINTSTS::Get().addr()] = fake_mmio::FakeMmioReg{};
    });
  }

  ScopedEndpointHaltMmio(const ScopedEndpointHaltMmio&) = delete;
  ScopedEndpointHaltMmio& operator=(const ScopedEndpointHaltMmio&) = delete;
};

}  // namespace

TEST_F(Dwc2Test, DisableEndpointMasksInterruptAndCompletesPendingRequests) {
  // Exercise IN (0x81 -> index 1), OUT (0x01 -> index 17), and high OUT (0x0F -> index 31, bit 31
  // of DAINTMSK).
  for (uint8_t ep_addr : {uint8_t{0x81}, uint8_t{0x01}, uint8_t{0x0F}}) {
    SCOPED_TRACE(static_cast<unsigned>(ep_addr));
    EndpointEventHandler event_handler;
    fdf::UnownedSynchronizedDispatcher ep_dispatcher = dut_.runtime().StartBackgroundDispatcher();
    auto cleanup_dispatcher = fit::defer([&] {
      libsync::Completion shutdown_complete;
      dut_.runtime().ShutdownBackgroundDispatcher(ep_dispatcher->get(),
                                                  [&] { shutdown_complete.Signal(); });
      shutdown_complete.Wait();
    });

    fidl::SharedClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client;

    const uint8_t ep_num = DWC_ADDR_TO_INDEX(ep_addr);
    const bool is_in = DWC_EP_IS_IN(ep_num);
    ScopedEndpointHaltMmio hw(dut_, ep_num);

    fidl::SyncClient dci_client = ConnectDciClient();
    ASSERT_TRUE(dci_client.is_valid());

    auto ep_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
    ASSERT_TRUE(ep_endpoints.is_ok());

    auto connect_result = dci_client->ConnectToEndpoint({ep_addr, std::move(ep_endpoints->server)});
    ASSERT_TRUE(connect_result.is_ok());

    ep_client.Bind(std::move(ep_endpoints->client), ep_dispatcher->async_dispatcher(),
                   &event_handler);

    ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, ep_addr));
    EXPECT_NE(hw.daintmsk.load() & (1u << ep_num), 0u);
    EXPECT_EQ(DEPCTL::Get(ep_num).FromValue(hw.depctl.load()).usbactep(), 1u);

    const uint32_t ahberr_bit = is_in ? DIEPINT::Get(ep_num).FromValue(0).set_ahberr(1).reg_value()
                                      : DOEPINT::Get(ep_num).FromValue(0).set_ahberr(1).reg_value();
    dut_.RunInDriverContext([&](Dwc2& driver) {
      auto* ep = GetEndpoint(driver, ep_num);
      ASSERT_NE(ep, nullptr);

      std::lock_guard<std::mutex> lock(ep->lock);
      ASSERT_TRUE(ep->enabled);
      ep->pending_zlp = true;
      hw.depctl.store(DEPCTL::Get(ep_num).FromValue(hw.depctl.load()).set_epena(1).reg_value());
      hw.depdma.store(0x12340000u);
      hw.epint.store(ahberr_bit);

      usb::FidlRequest active_req(usb::EndpointType::BULK);
      active_req->defer_completion(false);
      active_req.add_data(std::vector<uint8_t>(64, 0), 64, 0);
      ep->current_req.emplace(std::move(active_req));

      usb::FidlRequest queued_req(usb::EndpointType::BULK);
      queued_req->defer_completion(false);
      queued_req.add_data(std::vector<uint8_t>(128, 0), 128, 0);
      ep->queued_reqs.push(std::move(queued_req));
    });

    auto disable_result = dci_client->DisableEndpoint({ep_addr});
    ASSERT_TRUE(disable_result.is_ok()) << disable_result.error_value().FormatDescription();

    EXPECT_TRUE(hw.saw_epdis.load());
    if (!is_in) {
      EXPECT_TRUE(hw.saw_sgoutnak.load());
      EXPECT_EQ(DCTL::Get().FromValue(hw.dctl.load()).goutnaksts(), 0u);
    }
    EXPECT_EQ(hw.daintmsk.load() & (1u << ep_num), 0u);
    auto final_depctl = DEPCTL::Get(ep_num).FromValue(hw.depctl.load());
    EXPECT_EQ(final_depctl.usbactep(), 0u);
    EXPECT_EQ(final_depctl.snak(), 1u);
    EXPECT_EQ(final_depctl.epena(), 0u);
    EXPECT_EQ(final_depctl.epdis(), 0u);
    EXPECT_EQ(hw.depdma.load(), 0u);
    EXPECT_EQ(hw.epint.load(), ahberr_bit);

    dut_.RunInDriverContext([&](Dwc2& driver) {
      auto* ep = GetEndpoint(driver, ep_num);
      ASSERT_NE(ep, nullptr);

      std::lock_guard<std::mutex> lock(ep->lock);
      EXPECT_FALSE(ep->enabled);
      EXPECT_FALSE(ep->pending_zlp);
      EXPECT_FALSE(ep->current_req.has_value());
      EXPECT_TRUE(ep->queued_reqs.empty());
    });

    // Verify that active_req (submitted first) is completed before queued_req.
    EndpointEventHandler::CompletionRecord comp1 = event_handler.WaitForCompletion();
    EXPECT_EQ(comp1.status, ZX_ERR_IO_NOT_PRESENT);
    EXPECT_EQ(comp1.length, 64u);

    EndpointEventHandler::CompletionRecord comp2 = event_handler.WaitForCompletion();
    EXPECT_EQ(comp2.status, ZX_ERR_IO_NOT_PRESENT);
    EXPECT_EQ(comp2.length, 128u);
  }
}

TEST_F(Dwc2Test, CancelAllHaltsActiveEndpointWithoutDeactivating) {
  fidl::SyncClient dci_client = ConnectDciClient();
  ASSERT_TRUE(dci_client.is_valid());

  for (uint8_t ep_addr : {uint8_t{0x81}, uint8_t{0x01}}) {
    SCOPED_TRACE(static_cast<unsigned>(ep_addr));
    EndpointEventHandler event_handler;
    fdf::UnownedSynchronizedDispatcher ep_dispatcher = dut_.runtime().StartBackgroundDispatcher();
    auto cleanup_dispatcher = fit::defer([&] {
      libsync::Completion shutdown_complete;
      dut_.runtime().ShutdownBackgroundDispatcher(ep_dispatcher->get(),
                                                  [&] { shutdown_complete.Signal(); });
      shutdown_complete.Wait();
    });

    fidl::SharedClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client;

    const uint8_t ep_num = DWC_ADDR_TO_INDEX(ep_addr);
    const bool is_in = DWC_EP_IS_IN(ep_num);
    ScopedEndpointHaltMmio hw(dut_, ep_num);
    std::atomic<bool> tx_fifo_flushed = false;
    std::atomic<bool> rx_fifo_flushed = false;
    auto restore_grstctl = fit::defer([&] {
      dut_.RunInEnvironmentTypeContext([](Environment& env) { env.ResetGrstctlCallbacks(); });
    });
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.mmio()[GRSTCTL::Get().addr()].SetReadCallback(
          []() -> uint64_t { return GRSTCTL::Get().FromValue(0).set_ahbidle(1).reg_value(); });
      env.mmio()[GRSTCTL::Get().addr()].SetWriteCallback([&](uint64_t val) {
        auto reg = GRSTCTL::Get().FromValue(static_cast<uint32_t>(val));
        if (reg.txfflsh()) {
          tx_fifo_flushed.store(true);
        }
        if (reg.rxfflsh()) {
          rx_fifo_flushed.store(true);
        }
      });
    });

    auto ep_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
    ASSERT_TRUE(ep_endpoints.is_ok());
    ASSERT_TRUE(dci_client->ConnectToEndpoint({ep_addr, std::move(ep_endpoints->server)}).is_ok());
    ep_client.Bind(std::move(ep_endpoints->client), ep_dispatcher->async_dispatcher(),
                   &event_handler);

    ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, ep_addr));
    constexpr uint32_t kOtherEpBit = 1u << 2;
    hw.daintmsk.fetch_or((1u << DWC_EP0_IN) | (1u << DWC_EP0_OUT) | kOtherEpBit);
    // Simulate HandleReset masking non-EP0 endpoints concurrently during
    // StopEndpointTransferLocked.
    hw.reset_daintmsk_on_epdis.store((1u << DWC_EP0_IN) | (1u << DWC_EP0_OUT));

    dut_.RunInDriverContext([&](Dwc2& driver) {
      auto* ep = GetEndpoint(driver, ep_num);
      ASSERT_NE(ep, nullptr);

      std::lock_guard<std::mutex> lock(ep->lock);
      hw.depctl.store(DEPCTL::Get(ep_num).FromValue(hw.depctl.load()).set_epena(1).reg_value());
      hw.depdma.store(0x56780000u);

      usb::FidlRequest active_req(usb::EndpointType::BULK);
      active_req->defer_completion(false);
      active_req.add_data(std::vector<uint8_t>(64, 0), 64, 0);
      ep->current_req.emplace(std::move(active_req));

      usb::FidlRequest queued_req(usb::EndpointType::BULK);
      queued_req->defer_completion(false);
      queued_req.add_data(std::vector<uint8_t>(128, 0), 128, 0);
      ep->queued_reqs.push(std::move(queued_req));
    });

    auto cancel_result = dci_client->CancelAll({ep_addr});
    ASSERT_TRUE(cancel_result.is_ok()) << cancel_result.error_value().FormatDescription();

    EXPECT_TRUE(hw.saw_epdis.load());
    if (!is_in) {
      EXPECT_TRUE(hw.saw_sgoutnak.load());
      EXPECT_EQ(DCTL::Get().FromValue(hw.dctl.load()).goutnaksts(), 0u);
    }
    EXPECT_NE(hw.daintmsk.load() & (1u << ep_num), 0u);
    EXPECT_EQ(hw.daintmsk.load() & kOtherEpBit, 0u);
    auto final_depctl = DEPCTL::Get(ep_num).FromValue(hw.depctl.load());
    EXPECT_EQ(final_depctl.usbactep(), 1u);
    EXPECT_EQ(final_depctl.snak(), 1u);
    EXPECT_EQ(final_depctl.epena(), 0u);
    EXPECT_EQ(final_depctl.epdis(), 0u);
    EXPECT_EQ(hw.depdma.load(), 0u);
    EXPECT_EQ(tx_fifo_flushed.load(), is_in);
    EXPECT_FALSE(rx_fifo_flushed.load());

    dut_.RunInDriverContext([&](Dwc2& driver) {
      auto* ep = GetEndpoint(driver, ep_num);
      ASSERT_NE(ep, nullptr);
      std::lock_guard<std::mutex> lock(ep->lock);
      EXPECT_TRUE(ep->enabled);
      EXPECT_FALSE(ep->current_req.has_value());
      EXPECT_TRUE(ep->queued_reqs.empty());
    });

    EndpointEventHandler::CompletionRecord comp1 = event_handler.WaitForCompletion();
    EXPECT_EQ(comp1.status, ZX_ERR_IO_NOT_PRESENT);
    EXPECT_EQ(comp1.length, 64u);

    EndpointEventHandler::CompletionRecord comp2 = event_handler.WaitForCompletion();
    EXPECT_EQ(comp2.status, ZX_ERR_IO_NOT_PRESENT);
    EXPECT_EQ(comp2.length, 128u);
  }
}

TEST_F(Dwc2Test, DisableEndpointFlushesTxFifoForInOnly) {
  std::atomic<bool> tx_fifo_flushed = false;
  std::atomic<uint32_t> flushed_tx_fifo_num = 0;
  std::atomic<bool> rx_fifo_flushed = false;

  auto restore_grstctl = fit::defer([&] {
    dut_.RunInEnvironmentTypeContext([](Environment& env) { env.ResetGrstctlCallbacks(); });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.mmio()[GRSTCTL::Get().addr()].SetReadCallback(
        []() -> uint64_t { return GRSTCTL::Get().FromValue(0).set_ahbidle(1).reg_value(); });
    env.mmio()[GRSTCTL::Get().addr()].SetWriteCallback([&](uint64_t val) {
      auto write = GRSTCTL::Get().FromValue(static_cast<uint32_t>(val));
      if (write.txfflsh()) {
        tx_fifo_flushed.store(true);
        flushed_tx_fifo_num.store(write.txfnum());
      }
      if (write.rxfflsh()) {
        rx_fifo_flushed.store(true);
      }
    });
  });

  fidl::SyncClient dci_client = ConnectDciClient();
  ASSERT_TRUE(dci_client.is_valid());

  // IN endpoint: flushes its dedicated TX FIFO and does not flush the shared RX FIFO.
  constexpr uint8_t kInEpAddr = 0x81;
  ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, kInEpAddr));
  ASSERT_TRUE(dci_client->DisableEndpoint({kInEpAddr}).is_ok());
  EXPECT_TRUE(tx_fifo_flushed.load());
  EXPECT_EQ(flushed_tx_fifo_num.load(), DWC_ADDR_TO_INDEX(kInEpAddr));
  EXPECT_FALSE(rx_fifo_flushed.load());

  // OUT endpoint: does not flush the shared RX FIFO (or any TX FIFO).
  tx_fifo_flushed.store(false);
  rx_fifo_flushed.store(false);
  constexpr uint8_t kOutEpAddr = 0x01;
  ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, kOutEpAddr));
  ASSERT_TRUE(dci_client->DisableEndpoint({kOutEpAddr}).is_ok());
  EXPECT_FALSE(tx_fifo_flushed.load());
  EXPECT_FALSE(rx_fifo_flushed.load());
}

TEST_F(Dwc2Test, ConfigureEndpointClearsStaleInterrupts) {
  fidl::SyncClient dci_client = ConnectDciClient();
  ASSERT_TRUE(dci_client.is_valid());

  // IN endpoint: stale DIEPINT bits are cleared via W1C before DAINTMSK unmasks the endpoint.
  {
    constexpr uint8_t kInEpAddr = 0x81;
    const uint8_t ep_num = DWC_ADDR_TO_INDEX(kInEpAddr);
    std::atomic<uint32_t> diepint = UINT32_MAX;
    std::atomic<uint32_t> daintmsk = 0;
    auto cleanup_callbacks = fit::defer([&] {
      dut_.RunInEnvironmentTypeContext([&](Environment& env) {
        env.mmio()[DIEPINT::Get(ep_num).addr()] = fake_mmio::FakeMmioReg{};
        env.mmio()[DAINTMSK::Get().addr()] = fake_mmio::FakeMmioReg{};
      });
    });

    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.mmio()[DIEPINT::Get(ep_num).addr()].SetReadCallback(
          [&diepint]() -> uint64_t { return diepint.load(); });
      env.mmio()[DIEPINT::Get(ep_num).addr()].SetWriteCallback(
          [&diepint](uint64_t val) { diepint.fetch_and(~static_cast<uint32_t>(val)); });
      env.mmio()[DAINTMSK::Get().addr()].SetReadCallback(
          [&daintmsk]() -> uint64_t { return daintmsk.load(); });
      env.mmio()[DAINTMSK::Get().addr()].SetWriteCallback([&](uint64_t val) {
        if (val & (1u << ep_num)) {
          EXPECT_EQ(diepint.load(), 0u);
        }
        daintmsk.store(static_cast<uint32_t>(val));
      });
    });

    ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, kInEpAddr));
    EXPECT_EQ(diepint.load(), 0u);
    EXPECT_NE(daintmsk.load() & (1u << ep_num), 0u);
  }

  // OUT endpoint: stale DOEPINT bits are cleared via W1C before DAINTMSK unmasks the endpoint.
  {
    constexpr uint8_t kOutEpAddr = 0x01;
    const uint8_t ep_num = DWC_ADDR_TO_INDEX(kOutEpAddr);
    std::atomic<uint32_t> doepint = UINT32_MAX;
    std::atomic<uint32_t> daintmsk = 0;
    auto cleanup_callbacks = fit::defer([&] {
      dut_.RunInEnvironmentTypeContext([&](Environment& env) {
        env.mmio()[DOEPINT::Get(ep_num).addr()] = fake_mmio::FakeMmioReg{};
        env.mmio()[DAINTMSK::Get().addr()] = fake_mmio::FakeMmioReg{};
      });
    });

    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      env.mmio()[DOEPINT::Get(ep_num).addr()].SetReadCallback(
          [&doepint]() -> uint64_t { return doepint.load(); });
      env.mmio()[DOEPINT::Get(ep_num).addr()].SetWriteCallback(
          [&doepint](uint64_t val) { doepint.fetch_and(~static_cast<uint32_t>(val)); });
      env.mmio()[DAINTMSK::Get().addr()].SetReadCallback(
          [&daintmsk]() -> uint64_t { return daintmsk.load(); });
      env.mmio()[DAINTMSK::Get().addr()].SetWriteCallback([&](uint64_t val) {
        if (val & (1u << ep_num)) {
          EXPECT_EQ(doepint.load(), 0u);
        }
        daintmsk.store(static_cast<uint32_t>(val));
      });
    });

    ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, kOutEpAddr));
    EXPECT_EQ(doepint.load(), 0u);
    EXPECT_NE(daintmsk.load() & (1u << ep_num), 0u);
  }
}

TEST_F(Dwc2Test, StopEndpointSnakDoesNotRearmClearedEpEna) {
  fidl::SyncClient dci_client = ConnectDciClient();
  ASSERT_TRUE(dci_client.is_valid());

  constexpr uint8_t kInEpAddr = 0x81;
  const uint8_t ep_num = DWC_ADDR_TO_INDEX(kInEpAddr);
  ScopedEndpointHaltMmio hw(dut_, ep_num);

  ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, kInEpAddr));

  dut_.RunInDriverContext([&](Dwc2& driver) {
    auto* ep = GetEndpoint(driver, ep_num);
    ASSERT_NE(ep, nullptr);
    std::lock_guard<std::mutex> lock(ep->lock);
    hw.depctl.store(DEPCTL::Get(ep_num).FromValue(hw.depctl.load()).set_epena(1).reg_value());
    hw.depdma.store(0x11110000u);
    // Simulate hardware completing the transfer right after the initial DEPCTL read in
    // StopEndpointTransferLocked and before the SNAK RMW write lands.
    hw.clear_epena_on_first_depctl_read.store(true);
  });

  auto disable_result = dci_client->DisableEndpoint({kInEpAddr});
  ASSERT_TRUE(disable_result.is_ok()) << disable_result.error_value().FormatDescription();

  EXPECT_FALSE(hw.saw_epdis.load());
  auto final_depctl = DEPCTL::Get(ep_num).FromValue(hw.depctl.load());
  EXPECT_EQ(final_depctl.epena(), 0u);
  EXPECT_EQ(final_depctl.snak(), 1u);
  EXPECT_EQ(final_depctl.usbactep(), 0u);
  EXPECT_EQ(hw.depdma.load(), 0u);
}

TEST_F(Dwc2Test, HaltTimeoutQuarantinesInFlightRequestUntilResetCore) {
  fidl::SyncClient dci_client = ConnectDciClient();
  ASSERT_TRUE(dci_client.is_valid());

  for (bool use_disable : {true, false}) {
    for (uint8_t ep_addr : {uint8_t{0x81}, uint8_t{0x01}}) {
      SCOPED_TRACE(std::string(use_disable ? "DisableEndpoint:" : "CancelAll:") +
                   std::to_string(ep_addr));
      EndpointEventHandler event_handler;
      fdf::UnownedSynchronizedDispatcher ep_dispatcher = dut_.runtime().StartBackgroundDispatcher();
      auto cleanup_dispatcher = fit::defer([&] {
        libsync::Completion shutdown_complete;
        dut_.runtime().ShutdownBackgroundDispatcher(ep_dispatcher->get(),
                                                    [&] { shutdown_complete.Signal(); });
        shutdown_complete.Wait();
      });

      const uint8_t ep_num = DWC_ADDR_TO_INDEX(ep_addr);
      const bool is_in = DWC_EP_IS_IN(ep_num);

      auto ep_endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
      ASSERT_TRUE(ep_endpoints.is_ok());
      ASSERT_TRUE(
          dci_client->ConnectToEndpoint({ep_addr, std::move(ep_endpoints->server)}).is_ok());
      fidl::SharedClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(
          std::move(ep_endpoints->client), ep_dispatcher->async_dispatcher(), &event_handler);

      uint64_t base_pmo_count = 0;
      uint32_t expected_depdma = 0;
      {
        ScopedEndpointHaltMmio hw(dut_, ep_num);
        hw.fail_nakeff.store(true);
        hw.fail_epdis.store(true);

        ASSERT_NO_FATAL_FAILURE(ConfigureBulkEndpoint(dci_client, ep_addr));

        dut_.RunInDriverContext([&](Dwc2& driver) {
          base_pmo_count = GetBtiPmoCount(driver);
          EXPECT_EQ(GetQuarantinedRequestCount(driver), 0u);
          SetConfiguredState(driver, true);

          auto* ep = GetEndpoint(driver, ep_num);
          ASSERT_NE(ep, nullptr);
          std::lock_guard<std::mutex> lock(ep->lock);

          usb::FidlRequest active_req(usb::EndpointType::BULK);
          active_req->defer_completion(false);
          active_req.add_data(std::vector<uint8_t>(64, 0xAB), 64, 0);
          ep->queued_reqs.push(std::move(active_req));

          usb::FidlRequest queued_req(usb::EndpointType::BULK);
          queued_req->defer_completion(false);
          queued_req.add_data(std::vector<uint8_t>(128, 0xCD), 128, 0);
          ep->queued_reqs.push(std::move(queued_req));

          QueueNextRequest(driver, ep);
          EXPECT_TRUE(ep->current_req.has_value());
          EXPECT_EQ(ep->queued_reqs.size(), 1u);
          EXPECT_EQ(GetBtiPmoCount(driver), base_pmo_count + 1);
          expected_depdma = hw.depdma.load();
          EXPECT_NE(expected_depdma, 0u);
          EXPECT_EQ(DEPCTL::Get(ep_num).FromValue(hw.depctl.load()).epena(), 1u);
        });

        if (use_disable) {
          auto res = dci_client->DisableEndpoint({ep_addr});
          ASSERT_TRUE(res.is_ok()) << res.error_value().FormatDescription();
        } else {
          auto res = dci_client->CancelAll({ep_addr});
          ASSERT_TRUE(res.is_ok()) << res.error_value().FormatDescription();
        }

        EXPECT_TRUE(hw.saw_epdis.load());
        if (!is_in) {
          EXPECT_TRUE(hw.saw_sgoutnak.load());
          EXPECT_EQ(DCTL::Get().FromValue(hw.dctl.load()).goutnaksts(), 0u);
        }
        // EPEna remained stuck at 1: DEPDMA must not be zeroed, and the endpoint interrupt must
        // remain masked.
        EXPECT_EQ(DEPCTL::Get(ep_num).FromValue(hw.depctl.load()).epena(), 1u);
        EXPECT_EQ(hw.depdma.load(), expected_depdma);
        EXPECT_EQ(hw.daintmsk.load() & (1u << ep_num), 0u);

        // The unstarted queued_req (128 bytes) completes immediately with ZX_ERR_IO_NOT_PRESENT.
        EndpointEventHandler::CompletionRecord comp = event_handler.WaitForCompletion();
        EXPECT_EQ(comp.status, ZX_ERR_IO_NOT_PRESENT);
        EXPECT_EQ(comp.length, 128u);

        // Fence the endpoint channel and dispatcher to confirm active_req (64 bytes) was NOT
        // completed or unpinned while EPEna remains stuck.
        libsync::Completion ep_flushed;
        ep_client->GetInfo().ThenExactlyOnce([&](auto& info_res) {
          EXPECT_TRUE(info_res.is_error());
          ep_flushed.Signal();
        });
        ep_flushed.Wait();
        EXPECT_EQ(event_handler.PendingCompletionCount(), 0u);

        dut_.RunInDriverContext([&](Dwc2& driver) {
          auto* ep = GetEndpoint(driver, ep_num);
          ASSERT_NE(ep, nullptr);
          {
            std::lock_guard<std::mutex> lock(ep->lock);
            EXPECT_FALSE(ep->enabled);
            EXPECT_FALSE(ep->current_req.has_value());
            EXPECT_TRUE(ep->queued_reqs.empty());
          }
          EXPECT_EQ(GetQuarantinedRequestCount(driver), 1u);
          EXPECT_EQ(GetBtiPmoCount(driver), base_pmo_count + 1);
        });
      }

      // ResetCore() (invoked via StartController -> InitController) halts core DMA and releases the
      // quarantined request's pinned pages.
      auto start_res = dci_client->StartController();
      ASSERT_TRUE(start_res.is_ok()) << start_res.error_value().FormatDescription();

      dut_.RunInDriverContext([&](Dwc2& driver) {
        EXPECT_EQ(GetQuarantinedRequestCount(driver), 0u);
        EXPECT_EQ(GetBtiPmoCount(driver), base_pmo_count);
      });
    }
  }
}

}  // namespace dwc2
