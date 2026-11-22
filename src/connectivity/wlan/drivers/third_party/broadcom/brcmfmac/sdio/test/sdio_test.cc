/*
 * Copyright (c) 2019 The Fuchsia Authors
 *
 * Permission to use, copy, modify, and/or distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY
 * SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
 * OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
 * CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/sdio/sdio.h"

#include <fidl/fuchsia.hardware.gpio/cpp/wire.h>
#include <fuchsia/hardware/sdio/c/banjo.h>
#include <fuchsia/hardware/sdio/cpp/banjo-mock.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <cstring>
#include <memory>
#include <string_view>
#include <tuple>
#include <unordered_map>
#include <vector>

#include <sdk/lib/driver/testing/cpp/driver_runtime.h>
#include <zxtest/zxtest.h>

#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcm_hw_ids.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/brcmu_d11.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/bus.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/chip.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/common.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/device.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/proto.h"
#include "src/connectivity/wlan/drivers/third_party/broadcom/brcmfmac/test/stub_device.h"
#include "src/devices/gpio/testing/fake-gpio/fake-gpio.h"
#include "src/devices/lib/broadcom/commands.h"

// These numbers come from real bugs.
#define NOT_ALIGNED_SIZE 1541
#define ALIGNED_SIZE 1544

// This is required to use ddk::MockSdio.
bool operator==(const sdio_rw_txn_t& lhs, const sdio_rw_txn_t& rhs) { return false; }

zx::result<fuchsia_wlan_broadcom::WifiConfig> get_wifi_metadata(struct brcmf_bus* bus) {
  return bus->bus_priv.sdio->drvr->device->GetWifiConfig();
}

namespace {

class FakeSdioDevice : public wlan::brcmfmac::StubDevice {
 public:
  ~FakeSdioDevice() {
    libsync::Completion shutdown_complete;
    Shutdown([&] { shutdown_complete.Signal(); });
    shutdown_complete.Wait();
  }

  async_dispatcher_t* GetTimerDispatcher() override {
    return fdf::Dispatcher::GetCurrent()->async_dispatcher();
  }

  zx_status_t LoadFirmware(const char* path, zx_handle_t* fw, size_t* size) override {
    if (!enable_firmware_load_) {
      return StubDevice::LoadFirmware(path, fw, size);
    }
    const std::string_view filepath(path);
    if (filepath.ends_with(".clm_blob")) {
      return fail_clm_load_;
    }
    constexpr std::string_view kFakeNvram = "boardrev=0x1100\n";
    constexpr uint8_t kFakeFirmware[16] = {};
    const void* payload = filepath.ends_with(".txt") ? static_cast<const void*>(kFakeNvram.data())
                                                     : static_cast<const void*>(kFakeFirmware);
    const size_t payload_size = filepath.ends_with(".txt") ? kFakeNvram.size()
                                                           : sizeof(kFakeFirmware);
    zx::vmo vmo;
    zx_status_t status = zx::vmo::create(4096, 0, &vmo);
    if (status != ZX_OK) {
      return status;
    }
    status = vmo.write(payload, 0, payload_size);
    if (status != ZX_OK) {
      return status;
    }
    status = vmo.set_property(ZX_PROP_VMO_CONTENT_SIZE, &payload_size, sizeof(payload_size));
    if (status != ZX_OK) {
      return status;
    }
    *size = payload_size;
    firmware_vmos_.push_back(std::move(vmo));
    *fw = firmware_vmos_.back().get();
    return ZX_OK;
  }

  zx::result<fuchsia_wlan_broadcom::WifiConfig> GetWifiConfig() override {
    // Provide a fake implementation for this metadata.
    return zx::ok(fuchsia_wlan_broadcom::WifiConfig{{
        .oob_irq_mode = ZX_INTERRUPT_MODE_LEVEL_LOW,
        .clm_needed = clm_needed_,
        .cc_table = {fuchsia_wlan_broadcom::CcEntry{{
            .cc_abbr = "WW",
            .cc_rev = 0,
        }}},
    }});
  }

  bool enable_firmware_load_ = false;
  bool clm_needed_ = true;
  zx_status_t fail_clm_load_ = ZX_ERR_NOT_FOUND;

 private:
  std::vector<zx::vmo> firmware_vmos_;
};

class MockSdio : public ddk::MockSdio {
 public:
  // Override these methods because the generated methods in ddk::MockSdio require that
  // out_read_byte is non-null, which is only the case for reads. For writes it can/should be null.
  zx_status_t SdioDoRwByte(bool write, uint32_t addr, uint8_t write_byte,
                           uint8_t* out_read_byte) override {
    std::tuple<zx_status_t, uint8_t> ret = mock_do_rw_byte_.Call(write, addr, write_byte);
    if (out_read_byte) {
      *out_read_byte = std::get<1>(ret);
    }
    return std::get<0>(ret);
  }

  zx_status_t SdioDoVendorControlRwByte(bool write, uint8_t addr, uint8_t write_byte,
                                        uint8_t* out_read_byte) override {
    auto ret = mock_do_vendor_control_rw_byte_.Call(write, addr, write_byte);
    if (out_read_byte != nullptr) {
      *out_read_byte = std::get<1>(ret);
    }

    return std::get<0>(ret);
  }
};

class FakeSdioBus {
 public:
  static constexpr size_t kVmoSize = 4096;
  static constexpr size_t kVmoOffset = 0;
  static constexpr uint8_t kVmoId = 0;
  static constexpr uint32_t kFrameSize = 1500;
  static constexpr uint8_t kPortId = 0;

  static zx_status_t Create(std::unique_ptr<FakeSdioBus>* bus) {
    std::unique_ptr<FakeSdioBus> ptr(new FakeSdioBus());

    zx_status_t status = zx::vmo::create(kVmoSize, 0, &ptr->vmo_);
    if (status != ZX_OK) {
      return status;
    }

    zx_vaddr_t addr = 0;
    status = zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, kVmoOffset, ptr->vmo_,
                                        kVmoOffset, kVmoSize, &addr);
    if (status != ZX_OK) {
      return status;
    }
    ptr->mapped_vmo_addr_ = addr;

    {
      std::lock_guard lock(ptr->bus_.rx_tx_data.tx_space);
      wlan::drivers::components::Frame tx_frame(
          &ptr->bus_.rx_tx_data.tx_space, kVmoId, kVmoOffset, 0,
          reinterpret_cast<uint8_t*>(ptr->mapped_vmo_addr_.value()), kFrameSize, kPortId);
      ptr->bus_.rx_tx_data.tx_space.Store(std::move(tx_frame));
    }

    *bus = std::move(ptr);

    return ZX_OK;
  }

  ~FakeSdioBus() {
    if (mapped_vmo_addr_.has_value()) {
      zx_status_t status = zx::vmar::root_self()->unmap(mapped_vmo_addr_.value(), kVmoSize);
      if (status != ZX_OK) {
        BRCMF_ERR("Failed to unmap VMO: %s", zx_status_get_string(status));
      }
    }
  }

  void ExpectDoRwTxn(MockSdio& sdio, zx_status_t return_status, uint32_t addr, uint32_t size,
                     bool incr, bool write) {
    sdio.mock_do_rw_txn().ExpectCallWithMatcher([=](sdio_rw_txn_t txn) {
      EXPECT_EQ(txn.addr, addr);
      EXPECT_EQ(txn.write, write);
      EXPECT_EQ(txn.incr, incr);
      EXPECT_EQ(txn.buffers_count, 1);
      EXPECT_EQ(txn.buffers_list[0].offset, kVmoOffset);
      EXPECT_EQ(txn.buffers_list[0].size, size);
      EXPECT_EQ(txn.buffers_list[0].type, SDMMC_BUFFER_TYPE_VMO_ID);
      EXPECT_EQ(txn.buffers_list[0].buffer.vmo_id, kVmoId);

      return std::tuple<zx_status_t>{return_status};
    });
  }

  void ExpectWriteByte(MockSdio& sdio, zx_status_t return_status, uint32_t expected_addr,
                       uint8_t expected_write_byte) {
    sdio.mock_do_rw_byte().ExpectCallWithMatcher(
        [=](bool write, uint32_t actual_addr, uint8_t actual_write_byte) {
          EXPECT_EQ(true, write);
          EXPECT_EQ(expected_addr, actual_addr);
          EXPECT_EQ(expected_write_byte, actual_write_byte);
          return std::tuple<zx_status_t, uint8_t>{return_status, 0};
        });
  }

  brcmf_sdio* get() { return &bus_; }

 private:
  FakeSdioBus() = default;

  brcmf_sdio bus_ = {};
  zx::vmo vmo_;
  std::optional<zx_vaddr_t> mapped_vmo_addr_;
};

class SdioTest : public zxtest::Test {
  fdf_testing::ScopedGlobalLogger logging_;

 private:
  // Create a testing driver runtime to allow for the creation of fdf dispatchers.
  fdf_testing::DriverRuntime driver_runtime_;
};

TEST_F(SdioTest, IntrRegisterUnregister) {
  FakeSdioDevice device;
  brcmf_sdio_dev sdio_dev = {};
  sdio_func func1 = {};
  MockSdio sdio1;
  MockSdio sdio2;
  brcmf_bus bus_if = {};
  brcmf_mp_device settings = {};
  brcmf_sdio_pd sdio_settings = {};
  const struct brcmf_bus_ops sdio_bus_ops = {
      .get_wifi_metadata = get_wifi_metadata,
  };

  async::Loop fidl_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<fake_gpio::FakeGpio> wifi_gpio{fidl_loop.dispatcher(),
                                                                     std::in_place};
  EXPECT_OK(fidl_loop.StartThread("fidl-servers"));

  sdio_dev.func1 = &func1;
  auto wifi_gpio_client = wifi_gpio.SyncCall(&fake_gpio::FakeGpio::Connect);
  sdio_dev.fidl_gpios[WIFI_OOB_IRQ_GPIO_INDEX].Bind(std::move(wifi_gpio_client));
  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.sdio_proto_fn2 = *sdio2.GetProto();
  sdio_dev.drvr = device.drvr();
  bus_if.bus_priv.sdio = &sdio_dev;
  bus_if.ops = &sdio_bus_ops;
  sdio_dev.bus_if = &bus_if;
  sdio_dev.settings = &settings;
  sdio_dev.settings->bus.sdio = &sdio_settings;

  sdio1.ExpectEnableFnIntr(ZX_OK).ExpectDoVendorControlRwByte(
      ZX_OK, true, SDIO_CCCR_BRCM_SEPINT, SDIO_CCCR_BRCM_SEPINT_MASK | SDIO_CCCR_BRCM_SEPINT_OE, 0);
  sdio2.ExpectEnableFnIntr(ZX_OK);

  EXPECT_OK(brcmf_sdiod_intr_register(&sdio_dev, false));

  auto wifi_gpio_states = wifi_gpio.SyncCall(&fake_gpio::FakeGpio::GetStateLog);
  ASSERT_GT(wifi_gpio_states.size(), 0);
  EXPECT_TRUE(std::holds_alternative<fake_gpio::ReadSubState>(wifi_gpio_states.back().sub_state));
  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();

  // The interrupt handle handed out by FakeGpio is a duplicated handle. Closing the handle in
  // sdio_dev is not enough to make the IRQ thread stop. The handle in FakeGpio also needs to close.
  // Do this by destroying the FakeGpio instance.
  wifi_gpio.reset();

  // Unregister the interrupt.
  sdio1.ExpectDoVendorControlRwByte(ZX_OK, true, SDIO_CCCR_BRCM_SEPINT, 0, 0);
  sdio1.ExpectDisableFnIntr(ZX_OK);
  sdio2.ExpectDisableFnIntr(ZX_OK);

  brcmf_sdiod_intr_unregister(&sdio_dev);

  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();
}

TEST_F(SdioTest, IntrRegisterFwReload) {
  FakeSdioDevice device;
  brcmf_sdio_dev sdio_dev = {};
  sdio_func func1 = {};
  MockSdio sdio1;
  MockSdio sdio2;
  brcmf_bus bus_if = {};
  brcmf_mp_device settings = {};
  brcmf_sdio_pd sdio_settings = {};
  const struct brcmf_bus_ops sdio_bus_ops = {
      .get_wifi_metadata = get_wifi_metadata,
  };

  async::Loop fidl_loop{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<fake_gpio::FakeGpio> wifi_gpio{fidl_loop.dispatcher(),
                                                                     std::in_place};
  EXPECT_OK(fidl_loop.StartThread("fidl-servers"));
  sdio_dev.func1 = &func1;
  auto wifi_gpio_client = wifi_gpio.SyncCall(&fake_gpio::FakeGpio::Connect);
  sdio_dev.fidl_gpios[WIFI_OOB_IRQ_GPIO_INDEX].Bind(std::move(wifi_gpio_client));
  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.sdio_proto_fn2 = *sdio2.GetProto();
  sdio_dev.drvr = device.drvr();
  bus_if.bus_priv.sdio = &sdio_dev;
  bus_if.ops = &sdio_bus_ops;
  sdio_dev.bus_if = &bus_if;
  sdio_dev.settings = &settings;
  sdio_dev.settings->bus.sdio = &sdio_settings;

  sdio1.ExpectEnableFnIntr(ZX_OK).ExpectDoVendorControlRwByte(
      ZX_OK, true, SDIO_CCCR_BRCM_SEPINT, SDIO_CCCR_BRCM_SEPINT_MASK | SDIO_CCCR_BRCM_SEPINT_OE, 0);
  sdio2.ExpectEnableFnIntr(ZX_OK);

  EXPECT_OK(brcmf_sdiod_intr_register(&sdio_dev, true));

  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();
  zx_handle_close(sdio_dev.irq_handle);
}

TEST_F(SdioTest, IntrRegisterUnregisterNoMetadata) {
  FakeSdioDevice device;
  brcmf_sdio_dev sdio_dev = {};
  sdio_func func1 = {};
  brcmf_bus bus_if = {};
  brcmf_mp_device settings = {};
  brcmf_sdio_pd sdio_settings = {};
  // If the get metadata call fails with ZX_ERR_NOT_FOUND the register and unregister calls will
  // behave differently.
  const struct brcmf_bus_ops sdio_bus_ops = {
      .get_wifi_metadata = [](brcmf_bus*) -> zx::result<fuchsia_wlan_broadcom::WifiConfig> {
        return zx::error(ZX_ERR_NOT_FOUND);
      },
  };

  MockSdio sdio1;
  MockSdio sdio2;
  sdio_dev.func1 = &func1;
  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.sdio_proto_fn2 = *sdio2.GetProto();
  sdio_dev.drvr = device.drvr();
  bus_if.ops = &sdio_bus_ops;
  sdio_dev.bus_if = &bus_if;
  sdio_dev.settings = &settings;
  sdio_dev.settings->bus.sdio = &sdio_settings;

  // Register interrupt without metadata.
  sdio1.ExpectEnableFnIntr(ZX_OK);
  sdio2.ExpectEnableFnIntr(ZX_OK);

  EXPECT_OK(brcmf_sdiod_intr_register(&sdio_dev, false));

  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();

  // Unregister interrupt without metadata.
  sdio1.ExpectDisableFnIntr(ZX_OK);
  sdio2.ExpectDisableFnIntr(ZX_OK);

  brcmf_sdiod_intr_unregister(&sdio_dev);

  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();
}

TEST_F(SdioTest, VendorControl) {
  brcmf_sdio_dev sdio_dev = {};

  MockSdio sdio1;
  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();

  sdio1.ExpectDoVendorControlRwByte(ZX_ERR_IO, false, 0xf0, 0, 0xab)
      .ExpectDoVendorControlRwByte(ZX_OK, false, 0xf3, 0, 0x12)
      .ExpectDoVendorControlRwByte(ZX_ERR_BAD_STATE, true, 0xff, 0x55, 0)
      .ExpectDoVendorControlRwByte(ZX_ERR_TIMED_OUT, true, 0xfd, 0x79, 0);

  zx_status_t status;

  EXPECT_EQ(brcmf_sdiod_vendor_control_rb(&sdio_dev, 0xf0, &status), 0xab);
  EXPECT_EQ(status, ZX_ERR_IO);
  EXPECT_EQ(brcmf_sdiod_vendor_control_rb(&sdio_dev, 0xf3, nullptr), 0x12);

  brcmf_sdiod_vendor_control_wb(&sdio_dev, 0xff, 0x55, nullptr);
  brcmf_sdiod_vendor_control_wb(&sdio_dev, 0xfd, 0x79, &status);
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);

  sdio1.VerifyAndClear();
}

TEST_F(SdioTest, Transfer) {
  FakeSdioDevice device;
  std::unique_ptr<FakeSdioBus> sdio_bus;
  ASSERT_OK(FakeSdioBus::Create(&sdio_bus));
  brcmf_sdio_dev sdio_dev = {.drvr = device.drvr(), .bus = sdio_bus->get()};

  MockSdio sdio1;
  MockSdio sdio2;
  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.sdio_proto_fn2 = *sdio2.GetProto();

  sdio_bus->ExpectDoRwTxn(sdio1, ZX_OK, 0x458ef43b, FakeSdioBus::kFrameSize / 2, true, true);
  sdio_bus->ExpectDoRwTxn(sdio2, ZX_OK, 0x216977b9, FakeSdioBus::kFrameSize, true, true);

  uint8_t some_data[FakeSdioBus::kFrameSize] = {};

  EXPECT_OK(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x458ef43b, some_data,
                              FakeSdioBus::kFrameSize / 2, false));
  EXPECT_OK(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn2, 0x216977b9, some_data,
                              FakeSdioBus::kFrameSize, false));

  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();
}

TEST_F(SdioTest, IoAbort) {
  brcmf_sdio_dev sdio_dev = {};

  MockSdio sdio1;
  MockSdio sdio2;
  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.sdio_proto_fn2 = *sdio2.GetProto();

  sdio1.ExpectIoAbort(ZX_OK);
  sdio2.ExpectIoAbort(ZX_OK).ExpectIoAbort(ZX_OK).ExpectIoAbort(ZX_OK);

  EXPECT_OK(brcmf_sdiod_abort(&sdio_dev, 1));
  EXPECT_OK(brcmf_sdiod_abort(&sdio_dev, 2));
  EXPECT_OK(brcmf_sdiod_abort(&sdio_dev, 0));
  EXPECT_OK(brcmf_sdiod_abort(&sdio_dev, 200));

  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();
}

TEST_F(SdioTest, RamRw) {
  FakeSdioDevice device;
  std::unique_ptr<FakeSdioBus> sdio_bus;
  ASSERT_OK(FakeSdioBus::Create(&sdio_bus));
  brcmf_sdio_dev sdio_dev = {.drvr = device.drvr(), .bus = sdio_bus->get()};

  sdio_func func1 = {};
  pthread_mutex_init(&func1.lock, nullptr);

  MockSdio sdio1;

  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.func1 = &func1;

  sdio_bus->ExpectDoRwTxn(sdio1, ZX_OK, 0x0000ffe0, 0x00000020, true, true);
  sdio_bus->ExpectWriteByte(sdio1, ZX_OK, 0x0001000a, 0x80);
  sdio_bus->ExpectWriteByte(sdio1, ZX_OK, 0x0001000b, 0x00);
  sdio_bus->ExpectWriteByte(sdio1, ZX_OK, 0x0001000c, 0x00);
  sdio_bus->ExpectDoRwTxn(sdio1, ZX_OK, 0x00008000, 0x00000020, true, true);

  /* In this test the address is set to 0x000007fe0, and when running, this function
   will chunk the data which is originally 0x40 bytes big into two pieces to align
   the next transfer address to SBSDIO_SB_OFT_ADDR_LIMIT, which is 0x8000, each one
   is 0x20 bytes big. The first line above corresponding to the first piece, and the
   fifth line is the second piece, middle three are byte writes made in
   brcmf_sdiod_set_backplane_window().
   */
  uint8_t some_data[128] = {};
  EXPECT_OK(brcmf_sdiod_ramrw(&sdio_dev, true, 0x00007fe0, some_data, 0x00000040));
  sdio1.VerifyAndClear();
}

// This test case verifies that whether an error will returned when transfer size is
// not divisible by 4.
TEST_F(SdioTest, AlignSize) {
  FakeSdioDevice device;
  std::unique_ptr<FakeSdioBus> sdio_bus;
  ASSERT_OK(FakeSdioBus::Create(&sdio_bus));
  brcmf_sdio_dev sdio_dev = {.drvr = device.drvr(), .bus = sdio_bus->get()};
  sdio_func func1 = {};
  pthread_mutex_init(&func1.lock, nullptr);

  MockSdio sdio1;

  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.func1 = &func1;

  sdio_bus->ExpectDoRwTxn(sdio1, ZX_OK, 0x00008000, 0x00000020, true, true);

  uint8_t some_data[128] = {};
  // 4-byte-aligned size should succeed.
  EXPECT_OK(brcmf_sdiod_ramrw(&sdio_dev, true, 0x00000000, some_data, 0x00000020));
  // non-4-byte-aligned size for sending should fail and return ZX_ERR_INVALID_ARGS.
  EXPECT_EQ(brcmf_sdiod_ramrw(&sdio_dev, true, 0x00000000, some_data, 0x00000021),
            ZX_ERR_INVALID_ARGS);
  // non-4-byte-aligned size for receiving should fail and return ZX_ERR_INVALID_ARGS.
  EXPECT_EQ(brcmf_sdiod_ramrw(&sdio_dev, false, 0x00000000, some_data, 0x00000021),
            ZX_ERR_INVALID_ARGS);
  sdio1.VerifyAndClear();
}

zx_status_t fake_brcmf_schedule_recovery_worker(struct brcmf_pub* drvr) {
  drvr->drvr_resetting.store(true);
  return ZX_OK;
}

// Verify sdio_timeout recovery trigger logic in brcmf_sdiod_transfer_vmos().
TEST_F(SdioTest, SdioTimeoutRecoveryVmos) {
  FakeSdioDevice device;
  std::unique_ptr<FakeSdioBus> sdio_bus;
  ASSERT_OK(FakeSdioBus::Create(&sdio_bus));
  brcmf_sdio_dev sdio_dev = {.drvr = device.drvr(), .bus = sdio_bus->get()};
  sdio_func func1 = {};
  bool recovery_triggered = false;
  // Create RecoveryTrigger with the fake recovery worker.
  auto recovery_start_callback = std::make_shared<std::function<zx_status_t()>>();
  *recovery_start_callback = std::bind(&fake_brcmf_schedule_recovery_worker, device.drvr());
  device.drvr()->recovery_trigger =
      std::make_unique<wlan::brcmfmac::RecoveryTrigger>(recovery_start_callback);

  pthread_mutex_init(&func1.lock, nullptr);

  MockSdio sdio1;

  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.func1 = &func1;

  // The expected frame sequence: 4 frames with ZX_ERR_TIMED_OUT, 1 frame with ZX_OK, 5 frames with
  // ZX_ERR_TIMED_OUT.
  for (uint32_t i = 0; i < 4; i++) {
    sdio_bus->ExpectDoRwTxn(sdio1, ZX_ERR_TIMED_OUT, i * 0x00001000, FakeSdioBus::kFrameSize, true,
                            true);
  }

  sdio_bus->ExpectDoRwTxn(sdio1, ZX_OK, 0x00000000, FakeSdioBus::kFrameSize, true, true);

  for (uint32_t i = 0; i < 5; i++) {
    sdio_bus->ExpectDoRwTxn(sdio1, ZX_ERR_TIMED_OUT, i * 0x00001000, FakeSdioBus::kFrameSize, true,
                            true);
  }

  uint8_t some_data[FakeSdioBus::kFrameSize] = {};

  // Make sure the drvr_setting bit is false before any of the frames sent.
  recovery_triggered = device.drvr()->drvr_resetting.load();
  EXPECT_FALSE(recovery_triggered);

  // Write four frames and with ZX_ERR_TIMED_OUT returned.
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00000000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00001000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00002000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00003000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);

  // Write one frame with ZX_OK returned. This frame resets the sdio_timeout recovery trigger
  // counter.
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00000000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_OK);

  // Write one more frame with ZX_ERR_TIMED_OUT returned.
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00000000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);

  // Although there are already 5 ZX_ERR_TIMED_OUTs returned, the last frame resets the counter, so
  // no recovery should be triggered here.
  recovery_triggered = device.drvr()->drvr_resetting.load();
  EXPECT_FALSE(recovery_triggered);

  // Write four more frames with ZX_ERR_TIMED_OUT returned.
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00001000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00002000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00003000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);
  EXPECT_EQ(brcmf_sdiod_write(&sdio_dev, &sdio_dev.sdio_proto_fn1, 0x00004000, some_data,
                              FakeSdioBus::kFrameSize, false),
            ZX_ERR_TIMED_OUT);

  // sdio_timeout recovery should be triggered here since there are five contiguous ZX_ERR_TIMED_OUT
  // returned.
  recovery_triggered = device.drvr()->drvr_resetting.load();
  EXPECT_TRUE(recovery_triggered);

  sdio1.VerifyAndClear();
}

// Verify sdio_timeout recovery trigger logic in brcmf_sdiod_transfer_vmo().
TEST_F(SdioTest, SdioTimeoutRecoveryVmo) {
  FakeSdioDevice device;
  std::unique_ptr<FakeSdioBus> sdio_bus;
  ASSERT_OK(FakeSdioBus::Create(&sdio_bus));
  brcmf_sdio_dev sdio_dev = {.drvr = device.drvr(), .bus = sdio_bus->get()};

  {
    std::lock_guard lock(sdio_dev.bus->rx_tx_data.tx_space);
    sdio_dev.bus->rx_tx_data.tx_frame = std::move(*sdio_dev.bus->rx_tx_data.tx_space.Acquire());
  }
  sdio_func func1 = {};
  bool recovery_triggered = false;
  // Create RecoveryTrigger with the fake recovery worker.
  auto recovery_start_callback = std::make_shared<std::function<zx_status_t()>>();
  *recovery_start_callback = std::bind(&fake_brcmf_schedule_recovery_worker, device.drvr());
  device.drvr()->recovery_trigger =
      std::make_unique<wlan::brcmfmac::RecoveryTrigger>(recovery_start_callback);

  pthread_mutex_init(&func1.lock, nullptr);

  MockSdio sdio1;

  sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  sdio_dev.func1 = &func1;
  uint32_t some_data = 0x1234;
  uint32_t addr = 0x0000000;
  SBSDIO_FORMAT_ADDR(addr);

  // The expected frame sequence: 4 frames with ZX_ERR_TIMED_OUT, 1 frame with ZX_OK, 5 frames with
  // ZX_ERR_TIMED_OUT.
  for (size_t i = 0; i < 4; i++) {
    sdio_bus->ExpectDoRwTxn(sdio1, ZX_ERR_TIMED_OUT, addr, sizeof(some_data), true, true);
  }

  sdio_bus->ExpectDoRwTxn(sdio1, ZX_OK, addr, sizeof(some_data), true, true);

  for (size_t i = 0; i < 5; i++) {
    sdio_bus->ExpectDoRwTxn(sdio1, ZX_ERR_TIMED_OUT, addr, sizeof(some_data), true, true);
  }

  zx_status_t result = ZX_OK;
  // Make sure the drvr_setting bit is false before any of the frames sent.
  recovery_triggered = device.drvr()->drvr_resetting.load();
  EXPECT_FALSE(recovery_triggered);

  // Write four frames and with ZX_ERR_TIMED_OUT returned.
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);

  // Write one frame with ZX_OK returned. This frame resets the sdio_timeout recovery trigger
  // counter.
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_OK);

  // Write one more frame with ZX_ERR_TIMED_OUT returned.
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);

  // Although there are already 5 ZX_ERR_TIMED_OUTs returned, the last frame resets the counter, so
  // no recovery should be triggered here.
  recovery_triggered = device.drvr()->drvr_resetting.load();
  EXPECT_FALSE(recovery_triggered);

  // Write four more frames with ZX_ERR_TIMED_OUT returned.
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);
  brcmf_sdiod_func1_wl(&sdio_dev, 0x00000000, some_data, &result);
  EXPECT_EQ(result, ZX_ERR_TIMED_OUT);

  // sdio_timeout recovery should be triggered here since there are five contiguous ZX_ERR_TIMED_OUT
  // returned.
  recovery_triggered = device.drvr()->drvr_resetting.load();
  EXPECT_TRUE(recovery_triggered);

  sdio1.VerifyAndClear();
}

/*
 * A minimially initialized `struct brcmf_sdio` instance.
 *
 * This is contained in a class because `struct brcmf_sdio` holds pointers to various other structs.
 * To ensure memory is valid during usage, we package all of them together in this class.
 */
struct MinimalBrcmfSdio {
  MinimalBrcmfSdio(enum brcmf_sdiod_state sdiod_state, zx_duration_t ctl_done_timeout,
                   const char* workqueue_name, void (*work_item_handler)(WorkItem* work))
      : wq(workqueue_name) {
    sdio_dev = {.ctl_done_timeout = ctl_done_timeout, .drvr = &drvr, .state = sdiod_state};

    drvr.recovery_trigger = std::make_unique<wlan::brcmfmac::RecoveryTrigger>(nullptr);

    pthread_mutex_init(&func1.lock, nullptr);
    sdio_dev.func1 = &func1;

    bus_if.bus_priv.sdio = &sdio_dev;
    sdio_dev.bus_if = &bus_if;

    sdio_dev.bus = &bus;
    bus.sdiodev = &sdio_dev;

    bus.tx_queue = &tx_queue;
    bus.timer = &timer;

    // Prepare a WorkQueue with a single WorkItem to run the work_item_handler.
    bus.brcmf_wq = &wq;
    bus.datawork = WorkItem(work_item_handler);
    bus.dpc_triggered.store(false);
  }

  struct brcmf_pub drvr{};
  brcmf_sdio_dev sdio_dev{};
  sdio_func func1{};
  sdio_func func2{};
  struct brcmf_bus bus_if{};
  struct brcmf_sdio bus{};
  brcmf_tx_queue tx_queue{10};
  WorkQueue wq;

  // Fake timer that does nothing. This needs to exist for ResetClearsTxGlom test.
  Timer timer{fdf::Dispatcher::GetCurrent()->async_dispatcher(), [] {}, Timer::Type::OneShot};
};

/*
 * The sdio_bus_txctl_test() test helper calls brcmf_sdio_bus_txctl() by mocking the state around
 * the call using the function arguments. This helper returns the status returned by the
 * brcmf_sdio_bus_txctl() call.
 *
 *   sdiod_state       - state of the brcmf_sdio_dev contained in brcmf_bus
 *   ctl_done_timeout  - duration brcmf_sdio_bus_txctl() should wait for the
 *                       work_item_handler to complete before timing out
 *   workqueue_name    - name of the workqueue that effecively mocks dpc
 *   work_item_handler - WorkItem loaded into a WorkQueue to mock dpc
 *   expected_tx_ctlpkts - expected value of sdcnt.tx_ctlpkts after calling brcmf_sdio_bus_txctl()
 *   expected_tx_ctlerrs - expected value of sdcnt.tx_ctlerrs after calling brcmf_sdio_bus_txctl()
 *
 */
static zx_status_t sdio_bus_txctl_test(enum brcmf_sdiod_state sdiod_state,
                                       zx_duration_t ctl_done_timeout, const char* workqueue_name,
                                       void (*work_item_handler)(WorkItem* work),
                                       ulong expected_tx_ctlpkts, ulong expected_tx_ctlerrs) {
  MinimalBrcmfSdio b(sdiod_state, ctl_done_timeout, workqueue_name, work_item_handler);

  // Call brcmf_sdio_bus_txctl() with a blank message. Message processing is not mocked,
  // so this call purely tests
  unsigned char msg[] = "";
  const uint msglen = 0;
  zx_status_t status = brcmf_sdio_bus_txctl(&b.bus_if, msg, msglen);
  EXPECT_EQ(b.bus.sdcnt.tx_ctlpkts, expected_tx_ctlpkts);
  EXPECT_EQ(b.bus.sdcnt.tx_ctlerrs, expected_tx_ctlerrs);

  return status;
}

TEST_F(SdioTest, TxCtlSdioDown) {
  zx_status_t status = sdio_bus_txctl_test(
      BRCMF_SDIOD_DOWN, ZX_MSEC(CTL_DONE_TIMEOUT_MSEC), "brcmf_wq/txctl_sdio_down",
      [](WorkItem* work_item) {}, 0, 0);
  EXPECT_EQ(status, ZX_ERR_IO);
}

TEST_F(SdioTest, TxCtlOk) {
  zx_status_t status = sdio_bus_txctl_test(
      BRCMF_SDIOD_DATA, ZX_MSEC(CTL_DONE_TIMEOUT_MSEC), "brcmf_wq/txctl_ok",
      [](WorkItem* work) {
        struct brcmf_sdio* bus = containerof(work, struct brcmf_sdio, datawork);
        brcmf_sdio_if_ctrl_frame_stat_set(bus, [&bus]() {
          bus->ctrl_frame_err = ZX_OK;
          std::atomic_thread_fence(std::memory_order_seq_cst);
          brcmf_sdio_wait_event_wakeup(bus);
        });
      },
      1, 0);
  EXPECT_EQ(status, ZX_OK);
}

TEST_F(SdioTest, TxCtlTimeout) {
  zx_status_t status = sdio_bus_txctl_test(
      BRCMF_SDIOD_DATA, ZX_MSEC(1), "brcmf_wq/txctl_timeout", [](WorkItem* work) {}, 0, 1);
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);
}

TEST_F(SdioTest, TxCtlTimeoutUnexpectedCtrlFrameStatClear) {
  zx_status_t status = sdio_bus_txctl_test(
      BRCMF_SDIOD_DATA, ZX_MSEC(1), "brcmf_wq/txctl_timeout_unexpected_ctrl_frame_stat_clear",
      [](WorkItem* work) {
        struct brcmf_sdio* bus = containerof(work, struct brcmf_sdio, datawork);
        brcmf_sdio_if_ctrl_frame_stat_set(bus, [&bus]() { bus->ctrl_frame_err = ZX_OK; });
      },
      0, 1);
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);
}

TEST_F(SdioTest, TxCtlCtrlFrameStateNotCleared) {
  zx_status_t status = sdio_bus_txctl_test(
      BRCMF_SDIOD_DATA, ZX_MSEC(CTL_DONE_TIMEOUT_MSEC),
      "brcmf_wq/txctl_ctrl_frame_stat_not_cleared",
      [](WorkItem* work) {
        struct brcmf_sdio* bus = containerof(work, struct brcmf_sdio, datawork);
        std::atomic_thread_fence(std::memory_order_seq_cst);
        brcmf_sdio_wait_event_wakeup(bus);
      },
      0, 1);
  EXPECT_EQ(status, ZX_ERR_SHOULD_WAIT);
}

TEST_F(SdioTest, TxCtlCtrlFrameStateClearedWithError) {
  zx_status_t status = sdio_bus_txctl_test(
      BRCMF_SDIOD_DATA, ZX_MSEC(CTL_DONE_TIMEOUT_MSEC),
      "brcmf_wq/txctl_ctrl_frame_stat_cleared_with_error",
      [](WorkItem* work) {
        struct brcmf_sdio* bus = containerof(work, struct brcmf_sdio, datawork);
        brcmf_sdio_if_ctrl_frame_stat_set(bus, [&bus]() {
          bus->ctrl_frame_err = ZX_ERR_NO_MEMORY;
          std::atomic_thread_fence(std::memory_order_seq_cst);
          brcmf_sdio_wait_event_wakeup(bus);
        });
      },
      1, 0);
  EXPECT_EQ(status, ZX_ERR_NO_MEMORY);
}

TEST_F(SdioTest, ResetClearsTxGlom) {
  MinimalBrcmfSdio b(BRCMF_SDIOD_DATA, ZX_MSEC(1), "brcmf_wq/reset_clears_txglom",
                     [](WorkItem*) {});
  b.bus.txglom = true;
  brcmf_sdio_reset(&b.bus);
  ASSERT_FALSE(b.bus.txglom);
}

TEST_F(SdioTest, PrepareAndReleaseVmoClosesHandles) {
  MinimalBrcmfSdio b(BRCMF_SDIOD_DATA, ZX_MSEC(1), "brcmf_wq/prepare_and_release_vmo",
                     [](WorkItem*) {});
  MockSdio sdio1;
  MockSdio sdio2;
  b.sdio_dev.sdio_proto_fn1 = *sdio1.GetProto();
  b.sdio_dev.sdio_proto_fn2 = *sdio2.GetProto();

  constexpr uint8_t kVmoId = 1;
  constexpr size_t kVmoSize = 4096;
  uint8_t dummy_mapped_addr = 0;

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(kVmoSize, 0, &vmo));

  auto get_handle_count = [&](size_t* out_count) {
    zx_info_handle_count_t info{};
    ASSERT_OK(vmo.get_info(ZX_INFO_HANDLE_COUNT, &info, sizeof(info), nullptr, nullptr));
    *out_count = info.handle_count;
  };

  size_t handle_count = 0;
  ASSERT_NO_FATAL_FAILURE(get_handle_count(&handle_count));
  EXPECT_EQ(handle_count, 1u);

  zx::vmo fn1_registered_vmo;
  zx::vmo fn2_registered_vmo;
  sdio1.mock_register_vmo().ExpectCallWithMatcher(
      [&](uint32_t vmo_id, zx::vmo registered_vmo, uint64_t offset, uint64_t size,
          uint32_t vmo_rights) {
        EXPECT_EQ(vmo_id, kVmoId);
        EXPECT_TRUE(registered_vmo.is_valid());
        EXPECT_EQ(offset, 0u);
        EXPECT_EQ(size, kVmoSize);
        EXPECT_EQ(vmo_rights, SDMMC_VMO_RIGHT_READ | SDMMC_VMO_RIGHT_WRITE);
        fn1_registered_vmo = std::move(registered_vmo);
        return std::tuple<zx_status_t>{ZX_OK};
      });
  sdio2.mock_register_vmo().ExpectCallWithMatcher(
      [&](uint32_t vmo_id, zx::vmo registered_vmo, uint64_t offset, uint64_t size,
          uint32_t vmo_rights) {
        EXPECT_EQ(vmo_id, kVmoId);
        EXPECT_TRUE(registered_vmo.is_valid());
        EXPECT_EQ(offset, 0u);
        EXPECT_EQ(size, kVmoSize);
        EXPECT_EQ(vmo_rights, SDMMC_VMO_RIGHT_READ | SDMMC_VMO_RIGHT_WRITE);
        fn2_registered_vmo = std::move(registered_vmo);
        return std::tuple<zx_status_t>{ZX_OK};
      });

  ASSERT_OK(brcmf_sdio_prepare_vmo(&b.bus_if, kVmoId, vmo.get(), &dummy_mapped_addr, kVmoSize));
  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();

  // Both fn1 and fn2 now hold duplicated VMO handles in addition to `vmo`.
  ASSERT_NO_FATAL_FAILURE(get_handle_count(&handle_count));
  EXPECT_EQ(handle_count, 3u);

  sdio1.ExpectUnregisterVmo(ZX_OK, kVmoId, std::move(fn1_registered_vmo));
  sdio2.ExpectUnregisterVmo(ZX_OK, kVmoId, std::move(fn2_registered_vmo));

  ASSERT_OK(brcmf_sdio_release_vmo(&b.bus_if, kVmoId));
  sdio1.VerifyAndClear();
  sdio2.VerifyAndClear();

  // Both unregistered VMO handles returned by fn1 and fn2 must be closed by brcmf_sdio_release_vmo.
  ASSERT_NO_FATAL_FAILURE(get_handle_count(&handle_count));
  EXPECT_EQ(handle_count, 1u);
}

// Fake SDIO endpoint emulating a BCM4329 SiliconBackplane card so tests can run full
// brcmf_sdio_register(), brcmf_sdiod_probe(..., /*reloading=*/true), and brcmf_sdio_exit() flows.
class FakeReloadableSdio : public ddk::SdioProtocol<FakeReloadableSdio> {
 public:
  sdio_protocol_t GetProto() { return {.ops = &sdio_protocol_ops_, .ctx = this}; }

  ddk::SdioProtocolClient GetClient() {
    sdio_protocol_t proto = GetProto();
    return ddk::SdioProtocolClient(&proto);
  }

  zx_status_t SdioGetDevHwInfo(sdio_hw_info_t* out_hw_info) {
    *out_hw_info = {};
    out_hw_info->dev_hw_info.num_funcs = 3;
    out_hw_info->func_hw_info.manufacturer_id = 0x02d0;
    out_hw_info->func_hw_info.product_id = 0x4329;
    return ZX_OK;
  }

  zx_status_t SdioEnableFn() { return fail_enable_fn_; }
  zx_status_t SdioDisableFn() { return ZX_OK; }
  zx_status_t SdioEnableFnIntr() { return ZX_OK; }
  zx_status_t SdioDisableFnIntr() { return ZX_OK; }

  zx_status_t SdioUpdateBlockSize(uint16_t blk_sz, bool deflt) {
    if (fail_update_block_size_ != ZX_OK) {
      return fail_update_block_size_;
    }
    block_size_ = blk_sz;
    return ZX_OK;
  }

  zx_status_t SdioGetBlockSize(uint16_t* out_cur_blk_size) {
    if (fail_get_block_size_after_ == 0) {
      return fail_get_block_size_;
    }
    if (fail_get_block_size_after_ > 0) {
      --fail_get_block_size_after_;
    }
    *out_cur_blk_size = block_size_;
    return ZX_OK;
  }

  zx_status_t SdioDoRwByte(bool write, uint32_t addr, uint8_t write_byte, uint8_t* out_read_byte) {
    if (addr == SBSDIO_FUNC1_CHIPCLKCSR && fail_chipclkcsr_ != ZX_OK) {
      return fail_chipclkcsr_;
    }
    if (write) {
      uint8_t stored = write_byte;
      if (addr == SBSDIO_FUNC1_CHIPCLKCSR && !suppress_chipclkcsr_echo_) {
        // Advertise ALP/HT clock availability bits matching the requested clock bits.
        constexpr uint8_t kForceAlpOrReq = 0x01 | 0x08;
        constexpr uint8_t kForceHtOrReq = 0x02 | 0x10;
        constexpr uint8_t kAlpAvail = 0x40;
        constexpr uint8_t kHtAndAlpAvail = 0xc0;
        stored &= 0x3f;
        if (write_byte & kForceHtOrReq) {
          stored |= kHtAndAlpAvail;
        } else if (write_byte & kForceAlpOrReq) {
          stored |= kAlpAvail;
        }
      } else if (addr == SBSDIO_FUNC1_CHIPCLKCSR && suppress_chipclkcsr_echo_) {
        stored = 0;
      }
      f1_regs_[addr] = stored;
    } else if (out_read_byte) {
      auto it = f1_regs_.find(addr);
      *out_read_byte = (it != f1_regs_.end()) ? it->second : 0;
    }
    return ZX_OK;
  }

  zx_status_t SdioGetInBandIntr(zx::interrupt* out_irq) { return ZX_ERR_NOT_SUPPORTED; }
  void SdioAckInBandIntr() {}
  zx_status_t SdioIoAbort() { return ZX_OK; }
  zx_status_t SdioIntrPending(bool* out_pending) {
    *out_pending = false;
    return ZX_OK;
  }

  zx_status_t SdioDoVendorControlRwByte(bool write, uint8_t addr, uint8_t write_byte,
                                        uint8_t* out_read_byte) {
    if (fail_vendor_control_ != ZX_OK) {
      return fail_vendor_control_;
    }
    if (write) {
      vendor_regs_[addr] = write_byte;
    } else if (out_read_byte) {
      auto it = vendor_regs_.find(addr);
      *out_read_byte = (it != vendor_regs_.end()) ? it->second : 0;
    }
    return ZX_OK;
  }

  zx_status_t SdioRegisterVmo(uint32_t vmo_id, zx::vmo vmo, uint64_t offset, uint64_t size,
                              uint32_t vmo_rights) {
    vmos_[vmo_id] = std::move(vmo);
    return ZX_OK;
  }

  zx_status_t SdioUnregisterVmo(uint32_t vmo_id, zx::vmo* out_vmo) {
    auto it = vmos_.find(vmo_id);
    if (it == vmos_.end()) {
      return ZX_ERR_NOT_FOUND;
    }
    *out_vmo = std::move(it->second);
    vmos_.erase(it);
    return ZX_OK;
  }

  zx_status_t SdioDoRwTxn(const sdio_rw_txn_t* txn) {
    const uint32_t window = (static_cast<uint32_t>(f1_regs_[SBSDIO_FUNC1_SBADDRLOW]) << 8) |
                            (static_cast<uint32_t>(f1_regs_[SBSDIO_FUNC1_SBADDRMID]) << 16) |
                            (static_cast<uint32_t>(f1_regs_[SBSDIO_FUNC1_SBADDRHIGH]) << 24);
    uint32_t addr = (window & SBSDIO_SBWINDOW_MASK) | (txn->addr & SBSDIO_SB_OFT_ADDR_MASK);
    for (size_t i = 0; i < txn->buffers_count; ++i) {
      const auto& buf = txn->buffers_list[i];
      if ((buf.size % sizeof(uint32_t)) != 0) {
        return ZX_ERR_NOT_SUPPORTED;
      }
      auto vmo_it = vmos_.find(buf.buffer.vmo_id);
      if (vmo_it == vmos_.end()) {
        return ZX_ERR_BAD_HANDLE;
      }
      for (size_t offset = 0; offset < buf.size; offset += sizeof(uint32_t)) {
        if (txn->write) {
          uint32_t val = 0;
          zx_status_t status = vmo_it->second.read(&val, buf.offset + offset, sizeof(val));
          if (status != ZX_OK) {
            return status;
          }
          backplane_regs_[addr] = val;
          constexpr uint32_t kCm3SbTmStateLowAddr = 0x18002f98;
          constexpr uint32_t kTargetStateLowReset = 0x1;
          if (addr == kCm3SbTmStateLowAddr && (val & kTargetStateLowReset) == 0) {
            backplane_regs_[0xfffc] = 0x00002000;
          }
        } else {
          const uint32_t val = ReadBackplane32(addr);
          zx_status_t status = vmo_it->second.write(&val, buf.offset + offset, sizeof(val));
          if (status != ZX_OK) {
            return status;
          }
        }
        if (txn->incr) {
          addr += sizeof(uint32_t);
        }
      }
    }
    return ZX_OK;
  }

  zx_status_t SdioRequestCardReset() {
    ++card_reset_count_;
    return fail_card_reset_;
  }
  zx_status_t SdioPerformTuning() { return ZX_OK; }

  zx_status_t fail_update_block_size_ = ZX_OK;
  zx_status_t fail_get_block_size_ = ZX_OK;
  int fail_get_block_size_after_ = -1;
  zx_status_t fail_enable_fn_ = ZX_OK;
  zx_status_t fail_chipclkcsr_ = ZX_OK;
  zx_status_t fail_vendor_control_ = ZX_OK;
  zx_status_t fail_card_reset_ = ZX_OK;
  bool suppress_chipclkcsr_echo_ = false;
  uint32_t chip_id_ = BRCM_CC_4329_CHIP_ID;
  int card_reset_count_ = 0;

 private:
  uint32_t ReadBackplane32(uint32_t addr) const {
    constexpr uint32_t kSiEnumBase = 0x18000000;
    constexpr uint32_t kSocramCoreInfoAddr = 0x18003000;
    constexpr uint32_t kSocramOne64kBank = (1u << 4) | 2u;
    constexpr uint32_t kSbIdHighOffset = 0xffc;
    constexpr uint32_t kSbTmStateLowOffset = 0xf98;
    constexpr uint32_t kCoreRev5 = 5;
    constexpr uint32_t kTargetStateLowReset = 0x1;

    if (addr == kSiEnumBase) {
      return chip_id_;
    }
    if (addr == kSocramCoreInfoAddr) {
      return kSocramOne64kBank;
    }
    if (addr >= kSiEnumBase && (addr & 0xfff) == kSbIdHighOffset) {
      return kCoreRev5;
    }
    auto it = backplane_regs_.find(addr);
    if (it != backplane_regs_.end()) {
      return it->second;
    }
    if (addr >= kSiEnumBase && (addr & 0xfff) == kSbTmStateLowOffset) {
      return kTargetStateLowReset;
    }
    return 0;
  }

  uint16_t block_size_ = 0;
  std::unordered_map<uint32_t, uint8_t> f1_regs_;
  std::unordered_map<uint8_t, uint8_t> vendor_regs_;
  std::unordered_map<uint32_t, uint32_t> backplane_regs_;
  std::unordered_map<uint32_t, zx::vmo> vmos_;
};

void VerifyTxRejectedCleanlyWhileSdioDown(brcmf_pub* drvr) {
  uint8_t frame_buf[128] = {};
  wlan::drivers::components::Frame frame(nullptr, /*vmo_id=*/1, /*vmo_offset=*/0, /*buffer_id=*/0,
                                         frame_buf, sizeof(frame_buf), /*port_id=*/0);
  frame.ShrinkHead(64);
  EXPECT_EQ(brcmf_tx_queue_frames(drvr, cpp20::span(&frame, 1)), ZX_ERR_IO);
}

TEST_F(SdioTest, ReloadEarlyFailurePreservesBusAndProtoAndCompletesTx) {
  FakeSdioDevice device;
  FakeReloadableSdio sdio1;
  FakeReloadableSdio sdio2;
  fidl::WireSyncClient<fuchsia_hardware_gpio::Gpio> gpios[GPIO_COUNT] = {};
  ddk::SdioProtocolClient sdio_clients[SDIO_FN_COUNT] = {sdio1.GetClient(), sdio2.GetClient()};

  std::unique_ptr<brcmf_bus> bus_if;
  ASSERT_OK(brcmf_sdio_register(device.drvr(), gpios, sdio_clients, &bus_if));
  auto cleanup = fit::defer([&] {
    sdio1.fail_update_block_size_ = ZX_OK;
    sdio1.fail_enable_fn_ = ZX_OK;
    brcmf_sdio_exit(bus_if.get());
  });

  brcmf_sdio_dev* sdiodev = bus_if->bus_priv.sdio;
  brcmf_sdio* orig_bus = sdiodev->bus;
  ASSERT_NOT_NULL(orig_bus);
  brcmf_proto* orig_proto = device.drvr()->proto;
  brcmf_chip* orig_ci = orig_bus->ci;
  brcmf_core* orig_sdio_core = orig_bus->sdio_core;
  brcmf_core* orig_cc_core = sdiodev->cc_core;
  ASSERT_NOT_NULL(orig_proto);
  ASSERT_NOT_NULL(orig_ci);
  ASSERT_NOT_NULL(orig_sdio_core);
  ASSERT_NOT_NULL(orig_cc_core);
  ASSERT_EQ(sdiodev->state, BRCMF_SDIOD_DOWN);

  // Fail SetBlockSize on function 1 during reload.
  sdio1.fail_update_block_size_ = ZX_ERR_TIMED_OUT;
  EXPECT_EQ(brcmf_sdiod_probe(sdiodev, /*reloading=*/true), ZX_ERR_TIMED_OUT);
  sdio1.fail_update_block_size_ = ZX_OK;
  EXPECT_EQ(device.drvr()->proto, orig_proto);
  EXPECT_EQ(sdiodev->bus, orig_bus);
  EXPECT_EQ(orig_bus->ci, orig_ci);
  EXPECT_EQ(orig_bus->sdio_core, orig_sdio_core);
  EXPECT_EQ(sdiodev->cc_core, orig_cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());

  // Fail EnableFn on function 1 during reload.
  sdio1.fail_enable_fn_ = ZX_ERR_IO;
  EXPECT_EQ(brcmf_sdiod_probe(sdiodev, /*reloading=*/true), ZX_ERR_IO);
  sdio1.fail_enable_fn_ = ZX_OK;
  EXPECT_EQ(device.drvr()->proto, orig_proto);
  EXPECT_EQ(sdiodev->bus, orig_bus);
  EXPECT_EQ(orig_bus->ci, orig_ci);
  EXPECT_EQ(orig_bus->sdio_core, orig_sdio_core);
  EXPECT_EQ(sdiodev->cc_core, orig_cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());
}

TEST_F(SdioTest, ReloadProbeAttachFailurePreservesChipAndCompletesTx) {
  FakeSdioDevice device;
  FakeReloadableSdio sdio1;
  FakeReloadableSdio sdio2;
  fidl::WireSyncClient<fuchsia_hardware_gpio::Gpio> gpios[GPIO_COUNT] = {};
  ddk::SdioProtocolClient sdio_clients[SDIO_FN_COUNT] = {sdio1.GetClient(), sdio2.GetClient()};

  std::unique_ptr<brcmf_bus> bus_if;
  ASSERT_OK(brcmf_sdio_register(device.drvr(), gpios, sdio_clients, &bus_if));
  auto cleanup = fit::defer([&] {
    sdio1.fail_chipclkcsr_ = ZX_OK;
    sdio1.fail_vendor_control_ = ZX_OK;
    sdio1.suppress_chipclkcsr_echo_ = false;
    sdio1.chip_id_ = BRCM_CC_4329_CHIP_ID;
    sdio2.fail_get_block_size_after_ = -1;
    brcmf_sdio_exit(bus_if.get());
  });

  brcmf_sdio_dev* sdiodev = bus_if->bus_priv.sdio;
  brcmf_sdio* orig_bus = sdiodev->bus;
  ASSERT_NOT_NULL(orig_bus);
  brcmf_proto* orig_proto = device.drvr()->proto;
  brcmf_chip* orig_ci = orig_bus->ci;
  brcmf_core* orig_sdio_core = orig_bus->sdio_core;
  brcmf_core* orig_cc_core = sdiodev->cc_core;
  ASSERT_NOT_NULL(orig_proto);
  ASSERT_NOT_NULL(orig_ci);
  ASSERT_NOT_NULL(orig_sdio_core);
  ASSERT_NOT_NULL(orig_cc_core);
  ASSERT_EQ(sdiodev->state, BRCMF_SDIOD_DOWN);

  // 1. Fail ChipClkCSR write inside brcmf_sdio_probe_attach during brcmf_sdio_recovery.
  sdio1.fail_chipclkcsr_ = ZX_ERR_IO_REFUSED;
  EXPECT_EQ(brcmf_sdio_recovery(bus_if.get()), ZX_ERR_IO_NOT_PRESENT);
  sdio1.fail_chipclkcsr_ = ZX_OK;
  EXPECT_EQ(device.drvr()->proto, orig_proto);
  EXPECT_EQ(sdiodev->bus, orig_bus);
  EXPECT_EQ(orig_bus->ci, orig_ci);
  EXPECT_EQ(orig_bus->sdio_core, orig_sdio_core);
  EXPECT_EQ(sdiodev->cc_core, orig_cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());

  // 2. Fail ChipClkCSR readback mismatch inside brcmf_sdio_probe_attach.
  sdio1.suppress_chipclkcsr_echo_ = true;
  EXPECT_EQ(brcmf_sdiod_probe(sdiodev, /*reloading=*/true), ZX_ERR_IO_NOT_PRESENT);
  sdio1.suppress_chipclkcsr_echo_ = false;
  EXPECT_EQ(device.drvr()->proto, orig_proto);
  EXPECT_EQ(sdiodev->bus, orig_bus);
  EXPECT_EQ(orig_bus->ci, orig_ci);
  EXPECT_EQ(orig_bus->sdio_core, orig_sdio_core);
  EXPECT_EQ(sdiodev->cc_core, orig_cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());

  // 3. Fail brcmf_chip_attach (unsupported chip ID) and verify bus->ci / cores are not nulled out.
  sdio1.chip_id_ = 0xffff;
  EXPECT_EQ(brcmf_sdiod_probe(sdiodev, /*reloading=*/true), ZX_ERR_IO_NOT_PRESENT);
  sdio1.chip_id_ = BRCM_CC_4329_CHIP_ID;
  EXPECT_EQ(device.drvr()->proto, orig_proto);
  EXPECT_EQ(sdiodev->bus, orig_bus);
  EXPECT_EQ(orig_bus->ci, orig_ci);
  EXPECT_EQ(orig_bus->sdio_core, orig_sdio_core);
  EXPECT_EQ(sdiodev->cc_core, orig_cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());

  // 4. Fail GetBlockSize on fn2 inside brcmf_sdio_probe_attach after brcmf_chip_attach succeeds,
  // verifying the staged chip is detached and orig_ci / settings stay intact for later reloads.
  sdio2.fail_get_block_size_ = ZX_ERR_IO;
  sdio2.fail_get_block_size_after_ = 1;  // Succeed in brcmf_sdiod_set_block_size, fail in attach.
  EXPECT_EQ(brcmf_sdiod_probe(sdiodev, /*reloading=*/true), ZX_ERR_IO_NOT_PRESENT);
  sdio2.fail_get_block_size_after_ = -1;
  ASSERT_NOT_NULL(sdiodev->settings);
  EXPECT_NOT_NULL(sdiodev->settings->bus.sdio);
  EXPECT_EQ(orig_bus->ci, orig_ci);
  EXPECT_EQ(orig_bus->sdio_core, orig_sdio_core);
  EXPECT_EQ(sdiodev->cc_core, orig_cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());

  // 5. Fail vendor_control_rb after the new chip info is committed onto bus->ci, verifying
  // bus->ci stays valid (not double-detached on fail:) and a subsequent reload still succeeds.
  sdio1.fail_vendor_control_ = ZX_ERR_IO;
  EXPECT_EQ(brcmf_sdiod_probe(sdiodev, /*reloading=*/true), ZX_ERR_IO_NOT_PRESENT);
  sdio1.fail_vendor_control_ = ZX_OK;
  EXPECT_NOT_NULL(orig_bus->ci);
  EXPECT_NOT_NULL(orig_bus->sdio_core);
  EXPECT_NOT_NULL(sdiodev->cc_core);
  VerifyTxRejectedCleanlyWhileSdioDown(device.drvr());

  // 6. Subsequent reload succeeds cleanly.
  EXPECT_OK(brcmf_sdiod_probe(sdiodev, /*reloading=*/true));
  EXPECT_EQ(device.drvr()->proto, orig_proto);
  EXPECT_EQ(sdiodev->bus, orig_bus);
  EXPECT_NOT_NULL(orig_bus->ci);
  EXPECT_NOT_NULL(orig_bus->sdio_core);
  EXPECT_NOT_NULL(sdiodev->cc_core);
}

zx_status_t g_test_fail_ver_dcmd = ZX_OK;

zx_status_t StubQueryDcmd(brcmf_pub* drvr, int ifidx, uint cmd, void* buf, uint len,
                          bcme_status_t* fwerr) {
  if (fwerr) {
    *fwerr = BCME_OK;
  }
  if (cmd == BRCMF_C_GET_VAR && buf != nullptr && len >= 4 && std::memcmp(buf, "ver", 4) == 0) {
    if (g_test_fail_ver_dcmd != ZX_OK) {
      return g_test_fail_ver_dcmd;
    }
    constexpr char kVer[] = "wl0: version 1.0\n";
    std::memset(buf, 0, len);
    std::memcpy(buf, kVer, std::min<size_t>(len, sizeof(kVer)));
    return ZX_OK;
  }
  if (cmd == BRCMF_C_GET_VERSION && buf != nullptr && len >= sizeof(uint32_t)) {
    const uint32_t io_type = BRCMU_D11AC_IOTYPE;
    std::memcpy(buf, &io_type, sizeof(io_type));
    return ZX_OK;
  }
  if (buf != nullptr && len > 0) {
    std::memset(buf, 0, len);
  }
  return ZX_OK;
}

zx_status_t StubSetDcmd(brcmf_pub* drvr, int ifidx, uint cmd, void* buf, uint len,
                        bcme_status_t* fwerr) {
  if (fwerr) {
    *fwerr = BCME_OK;
  }
  return ZX_OK;
}

// Registers a bus on FakeReloadableSdio and starts it with fake firmware, so tests can run complete
// reloads.
class SdioReloadTest : public SdioTest {
 protected:
  void SetUp() override {
    device_.enable_firmware_load_ = true;
    device_.clm_needed_ = false;
    ASSERT_OK(fidl_loop_.StartThread("fidl-servers"));

    fidl::WireSyncClient<fuchsia_hardware_gpio::Gpio> gpios[GPIO_COUNT] = {};
    gpios[WIFI_OOB_IRQ_GPIO_INDEX].Bind(wifi_gpio_.SyncCall(&fake_gpio::FakeGpio::Connect));
    ddk::SdioProtocolClient sdio_clients[SDIO_FN_COUNT] = {sdio1_.GetClient(), sdio2_.GetClient()};
    ASSERT_OK(brcmf_sdio_register(device_.drvr(), gpios, sdio_clients, &bus_if_));
    orig_query_dcmd_ = device_.drvr()->proto->query_dcmd;
    orig_set_dcmd_ = device_.drvr()->proto->set_dcmd;
    device_.drvr()->proto->query_dcmd = StubQueryDcmd;
    device_.drvr()->proto->set_dcmd = StubSetDcmd;

    ASSERT_OK(brcmf_sdio_load_files(device_.drvr(), /*reload=*/false));
    ASSERT_OK(brcmf_bus_started(device_.drvr(), /*drvr_restarting=*/false));

    sdiodev_ = bus_if_->bus_priv.sdio;
    orig_config_ = device_.drvr()->config;
    orig_ifp_ = device_.drvr()->iflist[0];
    ASSERT_NOT_NULL(orig_config_);
    ASSERT_NOT_NULL(orig_ifp_);
    ASSERT_NO_FATAL_FAILURE(ExpectBusUp());
  }

  void TearDown() override {
    g_test_fail_ver_dcmd = ZX_OK;
    if (bus_if_) {
      device_.drvr()->proto->query_dcmd = orig_query_dcmd_;
      device_.drvr()->proto->set_dcmd = orig_set_dcmd_;
      brcmf_sdio_exit(bus_if_.get());
    }
  }

  void ExpectBusUp() {
    EXPECT_EQ(sdiodev_->state, BRCMF_SDIOD_DATA);
    EXPECT_EQ(bus_if_->state, BRCMF_BUS_UP);
    EXPECT_TRUE(sdiodev_->bus->wd_active.load());
    EXPECT_EQ(device_.drvr()->config, orig_config_);
    EXPECT_EQ(device_.drvr()->iflist[0], orig_ifp_);
  }

  void ExpectBusDownAndIntact() {
    EXPECT_EQ(sdiodev_->state, BRCMF_SDIOD_DOWN);
    EXPECT_EQ(bus_if_->state, BRCMF_BUS_DOWN);
    EXPECT_FALSE(sdiodev_->bus->wd_active.load());
    EXPECT_EQ(device_.drvr()->config, orig_config_);
    EXPECT_EQ(device_.drvr()->iflist[0], orig_ifp_);
    VerifyTxRejectedCleanlyWhileSdioDown(device_.drvr());
  }

  // Fails a reload after the firmware is running, in the two places that can happen, then checks
  // that the next reload succeeds. `reload` runs one reload attempt.
  void TestPostFirmwareFailures(fit::function<zx_status_t()> reload) {
    // Fail CLM load after brcmf_sdio_firmware_callback marks the bus up and starts the watchdog.
    device_.clm_needed_ = true;
    device_.fail_clm_load_ = ZX_ERR_NOT_FOUND;
    int card_resets = sdio1_.card_reset_count_;
    EXPECT_EQ(reload(), ZX_ERR_NOT_FOUND);
    // Once before the reload and once more after the failure, to quiet the running firmware.
    EXPECT_EQ(sdio1_.card_reset_count_, card_resets + 2);
    device_.clm_needed_ = false;
    ExpectBusDownAndIntact();

    // Fail preinit DCMD in brcmf_bus_started after brcmf_sdio_load_files succeeds.
    g_test_fail_ver_dcmd = ZX_ERR_TIMED_OUT;
    card_resets = sdio1_.card_reset_count_;
    EXPECT_EQ(reload(), ZX_ERR_TIMED_OUT);
    EXPECT_EQ(sdio1_.card_reset_count_, card_resets + 2);
    g_test_fail_ver_dcmd = ZX_OK;
    ExpectBusDownAndIntact();

    // Subsequent reload brings the bus back up cleanly.
    card_resets = sdio1_.card_reset_count_;
    EXPECT_OK(reload());
    EXPECT_EQ(sdio1_.card_reset_count_, card_resets + 1);
    ExpectBusUp();
  }

  FakeSdioDevice device_;
  FakeReloadableSdio sdio1_;
  FakeReloadableSdio sdio2_;
  async::Loop fidl_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<fake_gpio::FakeGpio> wifi_gpio_{fidl_loop_.dispatcher(),
                                                                      std::in_place};
  std::unique_ptr<brcmf_bus> bus_if_;
  brcmf_sdio_dev* sdiodev_ = nullptr;
  brcmf_cfg80211_info* orig_config_ = nullptr;
  brcmf_if* orig_ifp_ = nullptr;
  decltype(brcmf_proto::query_dcmd) orig_query_dcmd_ = nullptr;
  decltype(brcmf_proto::set_dcmd) orig_set_dcmd_ = nullptr;
};

TEST_F(SdioReloadTest, RecoveryPostFirmwareFailureResetsBusStateAndRecovers) {
  TestPostFirmwareFailures([this] { return brcmf_sdio_recovery(bus_if_.get()); });
}

// Device::Reset reloads through bus suspend and resume instead of recovery.
TEST_F(SdioReloadTest, ResumePostFirmwareFailureResetsBusStateAndRecovers) {
  TestPostFirmwareFailures([this] {
    EXPECT_OK(brcmf_bus_suspend(bus_if_.get()));
    return brcmf_bus_resume(bus_if_.get());
  });
}

TEST_F(SdioReloadTest, ResumeProbeFailureKeepsBusDown) {
  ASSERT_OK(brcmf_bus_suspend(bus_if_.get()));
  sdio1_.fail_chipclkcsr_ = ZX_ERR_IO_REFUSED;
  EXPECT_EQ(brcmf_bus_resume(bus_if_.get()), ZX_ERR_IO_NOT_PRESENT);
  sdio1_.fail_chipclkcsr_ = ZX_OK;
  ExpectBusDownAndIntact();

  ASSERT_OK(brcmf_bus_suspend(bus_if_.get()));
  EXPECT_OK(brcmf_bus_resume(bus_if_.get()));
  ExpectBusUp();
}

// Device::Reset resumes even when suspend failed or was skipped, so resume must not rely on suspend
// having reset the card and taken the reload lock.
TEST_F(SdioReloadTest, ResumeAfterFailedOrSkippedSuspend) {
  // Suspend was skipped.
  int card_resets = sdio1_.card_reset_count_;
  EXPECT_OK(brcmf_bus_resume(bus_if_.get()));
  EXPECT_EQ(sdio1_.card_reset_count_, card_resets + 1);
  ExpectBusUp();

  // Suspend failed and the card still does not reset.
  sdio1_.fail_card_reset_ = ZX_ERR_IO;
  EXPECT_EQ(brcmf_bus_suspend(bus_if_.get()), ZX_ERR_IO);
  EXPECT_EQ(brcmf_bus_resume(bus_if_.get()), ZX_ERR_IO);
  sdio1_.fail_card_reset_ = ZX_OK;
  ExpectBusDownAndIntact();

  // Suspend failed and the card resets by the time resume runs.
  sdio1_.fail_card_reset_ = ZX_ERR_IO;
  EXPECT_EQ(brcmf_bus_suspend(bus_if_.get()), ZX_ERR_IO);
  sdio1_.fail_card_reset_ = ZX_OK;
  EXPECT_OK(brcmf_bus_resume(bus_if_.get()));
  ExpectBusUp();
}

// A failed first attach must free the bus exactly once.
TEST_F(SdioTest, RegisterFailsCleanlyWhenChipDoesNotRespond) {
  FakeSdioDevice device;
  FakeReloadableSdio sdio1;
  FakeReloadableSdio sdio2;
  fidl::WireSyncClient<fuchsia_hardware_gpio::Gpio> gpios[GPIO_COUNT] = {};
  ddk::SdioProtocolClient sdio_clients[SDIO_FN_COUNT] = {sdio1.GetClient(), sdio2.GetClient()};

  sdio1.fail_chipclkcsr_ = ZX_ERR_IO_REFUSED;
  std::unique_ptr<brcmf_bus> bus_if;
  EXPECT_EQ(brcmf_sdio_register(device.drvr(), gpios, sdio_clients, &bus_if),
            ZX_ERR_IO_NOT_PRESENT);
  EXPECT_NULL(bus_if);
}
}  // namespace
