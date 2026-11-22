// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fpromise/single_threaded_executor.h>
#include <lib/sync/cpp/completion.h>

#include <atomic>

#include <fbl/unaligned.h>

#include "src/devices/block/drivers/ufs/device_manager.h"
#include "src/devices/block/drivers/ufs/registers.h"
#include "unit-lib.h"

namespace ufs {
using namespace ufs_mock_device;

class InitTest : public UfsTest {
 public:
  void SetUp() override { InitMockDevice(); }
};

TEST_F(InitTest, Basic) { ASSERT_NO_FATAL_FAILURE(StartDriver()); }

TEST_F(InitTest, DeviceNotFound) {
  // Override kDmeLinkStartUp handler to complete without setting device_present.
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeLinkStartUp,
      [](ufs_mock_device::UfsMockDevice&, uint32_t, uint32_t, uint32_t) {});

  TestUfs::SetMockDevice(&mock_device_);

  zx::result result = driver_test().StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    args.config(fake_config.ToVmo());
  });
  ASSERT_TRUE(result.is_error());
  ASSERT_EQ(result.status_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(InitTest, LinkStartupFailure) {
  // Override kDmeLinkStartUp handler to always fail.
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeLinkStartUp,
      [](ufs_mock_device::UfsMockDevice& mock_device, uint32_t, uint32_t, uint32_t) {
        UicCommandArgument2Reg::Get()
            .ReadFrom(mock_device.GetRegisters())
            .set_result_code(UicCommandArgument2Reg::GenericErrorCode::kFailure)
            .WriteTo(mock_device.GetRegisters());
      });

  TestUfs::SetMockDevice(&mock_device_);

  zx::result result = driver_test().StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    args.config(fake_config.ToVmo());
  });
  ASSERT_TRUE(result.is_error());
  ASSERT_EQ(result.status_value(), ZX_ERR_INTERNAL);
}

TEST_F(InitTest, GetControllerDescriptor) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bLength, sizeof(DeviceDescriptor));
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bDescriptorIDN,
            static_cast<uint8_t>(DescriptorType::kDevice));
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bDeviceSubClass, 0x01);
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bNumberWLU, 0x04);
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bInitPowerMode, 0x01);
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bHighPriorityLUN, 0x7F);
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().wSpecVersion, htobe16(0x0310));
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bUD0BaseOffset, 0x16);
  EXPECT_EQ(dut_->GetDeviceManager().GetDeviceDescriptor().bUDConfigPLength, 0x1A);

  EXPECT_EQ(dut_->GetDeviceManager().GetGeometryDescriptor().bLength, sizeof(GeometryDescriptor));
  EXPECT_EQ(dut_->GetDeviceManager().GetGeometryDescriptor().bDescriptorIDN,
            static_cast<uint8_t>(DescriptorType::kGeometry));
  EXPECT_EQ(fbl::UnalignedLoad<uint64_t>(
                &dut_->GetDeviceManager().GetGeometryDescriptor().qTotalRawDeviceCapacity),
            htobe64(kMockTotalDeviceCapacity >> 9));
  EXPECT_EQ(dut_->GetDeviceManager().GetGeometryDescriptor().bMaxNumberLU, 0x01);
}

TEST_F(InitTest, AddLogicalUnits) {
  constexpr uint8_t kDefualtLunCount = 1;
  constexpr uint8_t kMaxLunCount = 8;

  for (uint8_t lun = kDefualtLunCount; lun < kMaxLunCount; ++lun) {
    mock_device_.AddLun(lun);
  }

  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_EQ(dut_->GetLogicalUnitCount(), kMaxLunCount);
}

TEST_F(InitTest, LogicalUnitBlockInfo) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  scsi::BlockDevice* block_device = nullptr;
  while (true) {
    {
      std::lock_guard<std::mutex> lock(dut_->lock());
      if (!dut_->block_devs().empty() && dut_->block_devs().contains(0) &&
          !dut_->block_devs().at(0).empty()) {
        block_device = dut_->block_devs().at(0).at(0).get();
        break;
      }
    }
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }

  ASSERT_EQ(block_device->block_size_bytes(), kMockBlockSize);
  ASSERT_EQ(block_device->block_count(), kMockTotalDeviceCapacity / kMockBlockSize);
}

TEST_F(InitTest, UnitAttentionClear) {
  mock_device_.SetUnitAttention(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
}

TEST_F(InitTest, Inspect) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  for (const char* prop_name :
       {"block.read.count", "block.write.count", "block.flush.count", "block.trim.count",
        "block.read.kib", "block.write.kib", "block.flush.kib", "block.trim.kib"}) {
    auto prop = ufs->node().get_property<inspect::UintPropertyValue>(prop_name);
    ASSERT_NE(prop, nullptr);
    EXPECT_EQ(prop->value(), 0U);
  }

  const auto* version = ufs->GetByPath({"version"});
  ASSERT_NE(version, nullptr);
  auto major_version =
      version->node().get_property<inspect::UintPropertyValue>("major_version_number");
  ASSERT_NE(major_version, nullptr);
  EXPECT_EQ(major_version->value(), kMajorVersion);
  auto minor_version =
      version->node().get_property<inspect::UintPropertyValue>("minor_version_number");
  ASSERT_NE(minor_version, nullptr);
  EXPECT_EQ(minor_version->value(), kMinorVersion);
  auto version_suffix = version->node().get_property<inspect::UintPropertyValue>("version_suffix");
  ASSERT_NE(version_suffix, nullptr);
  EXPECT_EQ(version_suffix->value(), kVersionSuffix);

  const auto* controller = ufs->GetByPath({"controller"});
  ASSERT_NE(controller, nullptr);
  auto logical_unit_count =
      controller->node().get_property<inspect::UintPropertyValue>("logical_unit_count");
  ASSERT_NE(logical_unit_count, nullptr);
  EXPECT_EQ(logical_unit_count->value(), 1U);
  auto reference_clock =
      controller->node().get_property<inspect::StringPropertyValue>("reference_clock");
  ASSERT_NE(reference_clock, nullptr);
  EXPECT_EQ(reference_clock->value(), "38.4 MHz");

  const auto* attributes = controller->GetByPath({"attributes"});
  ASSERT_NE(attributes, nullptr);
  auto boot_lun_enabled = attributes->node().get_property<inspect::UintPropertyValue>("bBootLunEn");
  ASSERT_NE(boot_lun_enabled, nullptr);
  EXPECT_EQ(boot_lun_enabled->value(), 0x01U);

  const auto* unipro = controller->GetByPath({"unipro"});
  ASSERT_NE(unipro, nullptr);
  auto local_version = unipro->node().get_property<inspect::UintPropertyValue>("local_version");
  ASSERT_NE(local_version, nullptr);
  EXPECT_EQ(local_version->value(), kUniproVersion);
}

class ReferenceClockTest : public InitTest {
 public:
  void SetUp() override {
    InitTest::SetUp();
    mock_device_.GetQueryRequestProcessor().SetHook(
        QueryOpcode::kWriteAttribute,
        [this](UfsMockDevice& mock_device, QueryRequestUpiuData& req_upiu,
               QueryResponseUpiuData& rsp_upiu) {
          if (req_upiu.idn == static_cast<uint8_t>(Attributes::bRefClkFreq)) {
            reference_clock_writes_.fetch_add(1);
          }
          return mock_device_.GetQueryRequestProcessor().DefaultWriteAttributeHandler(
              mock_device, req_upiu, rsp_upiu);
        });
  }

 protected:
  static constexpr uint32_t kRefClk19_2MHz =
      static_cast<uint32_t>(AttributeReferenceClock::k19_2MHz);
  static constexpr uint32_t kRefClk38_4MHz =
      static_cast<uint32_t>(AttributeReferenceClock::k38_4MHz);

  std::atomic<uint32_t> reference_clock_writes_{0};
};

// bRefClkFreq is a persistent attribute whose writes may be slow, so it is only written when the
// device reports a different frequency than the one the host provides.
TEST_F(ReferenceClockTest, WrittenWhenDifferent) {
  mock_device_.SetAttribute(Attributes::bRefClkFreq, kRefClk19_2MHz);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_EQ(reference_clock_writes_.load(), 1u);
  EXPECT_EQ(mock_device_.GetAttribute(Attributes::bRefClkFreq), kRefClk38_4MHz);
}

TEST_F(ReferenceClockTest, NotWrittenWhenUnchanged) {
  mock_device_.SetAttribute(Attributes::bRefClkFreq, kRefClk38_4MHz);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_EQ(reference_clock_writes_.load(), 0u);
  EXPECT_EQ(mock_device_.GetAttribute(Attributes::bRefClkFreq), kRefClk38_4MHz);
}

TEST_F(ReferenceClockTest, WrittenWhenReadFails) {
  mock_device_.SetAttribute(Attributes::bRefClkFreq, kRefClk38_4MHz);
  mock_device_.GetQueryRequestProcessor().SetHook(
      QueryOpcode::kReadAttribute,
      [this](UfsMockDevice& mock_device, QueryRequestUpiuData& req_upiu,
             QueryResponseUpiuData& rsp_upiu) {
        if (req_upiu.idn == static_cast<uint8_t>(Attributes::bRefClkFreq)) {
          rsp_upiu.header.response = UpiuHeaderResponseCode::kTargetFailure;  // Error injection
          return ZX_OK;
        }
        return mock_device_.GetQueryRequestProcessor().DefaultReadAttributeHandler(
            mock_device, req_upiu, rsp_upiu);
      });
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_EQ(reference_clock_writes_.load(), 1u);
}

TEST_F(InitTest, WriteBoosterIsSupportedSharedBuffer) {
  // Shared buffer Type
  mock_device_.GetDeviceDesc().bWriteBoosterBufferType =
      static_cast<uint8_t>(WriteBoosterBufferType::kSharedBuffer);
  mock_device_.GetDeviceDesc().dNumSharedWriteBoosterBufferAllocUnits = betoh32(1);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_TRUE(dut_->GetDeviceManager().IsWriteBoosterEnabled());
}

TEST_F(InitTest, WriteBoosterIsSupportedDedicatedBuffer) {
  // LU dedicated buffer Type
  mock_device_.GetDeviceDesc().bWriteBoosterBufferType =
      static_cast<uint8_t>(WriteBoosterBufferType::kLuDedicatedBuffer);
  mock_device_.GetLogicalUnit(0).GetUnitDesc().dLUNumWriteBoosterBufferAllocUnits = betoh32(1);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_TRUE(dut_->GetDeviceManager().IsWriteBoosterEnabled());
}

TEST_F(InitTest, WriteBoosterIsNotSupported) {
  // WriteBooster is not supported.
  ExtendedUfsFeaturesSupport ext_feature_support;
  ext_feature_support.set_writebooster_support(false);
  mock_device_.GetDeviceDesc().dExtendedUfsFeaturesSupport = ext_feature_support.value;
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_FALSE(dut_->GetDeviceManager().IsWriteBoosterEnabled());
}

TEST_F(InitTest, WriteBoosterZeroAllocUnits) {
  // Zero alloc units
  mock_device_.GetDeviceDesc().bWriteBoosterBufferType =
      static_cast<uint8_t>(WriteBoosterBufferType::kLuDedicatedBuffer);
  mock_device_.GetLogicalUnit(0).GetUnitDesc().dLUNumWriteBoosterBufferAllocUnits = betoh32(0);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_FALSE(dut_->GetDeviceManager().IsWriteBoosterEnabled());
}

TEST_F(InitTest, WriteBoosterBufferLifeTime) {
  // Exceeds buffer life time
  mock_device_.SetAttribute(Attributes::bWBBufferLifeTimeEst, kExceededWriteBoosterBufferLifeTime);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_FALSE(dut_->GetDeviceManager().IsWriteBoosterEnabled());
}

TEST_F(InitTest, PowerOnWriteProtectEnable) {
  mock_device_.SetFlag(Flags::fPowerOnWPEn, true);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_TRUE(dut_->GetDeviceManager().IsPowerOnWritePotectEnabled());
}

TEST_F(InitTest, PowerOnWriteProtectDisable) {
  mock_device_.SetFlag(Flags::fPowerOnWPEn, false);
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_FALSE(dut_->GetDeviceManager().IsPowerOnWritePotectEnabled());
}

TEST_F(InitTest, LogicalLunPowerOnWriteProtectEnable) {
  uint8_t lun = 0;
  mock_device_.SetFlag(Flags::fPowerOnWPEn, true);
  mock_device_.GetLogicalUnit(lun).GetUnitDesc().bLUWriteProtect =
      LUWriteProtect::kPowerOnWriteProtect;
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_TRUE(dut_->GetDeviceManager().IsLogicalLunPowerOnWriteProtect());
}

TEST_F(InitTest, LogicalLunPowerOnWriteProtectDisable) {
  uint8_t lun = 0;
  mock_device_.SetFlag(Flags::fPowerOnWPEn, true);
  mock_device_.GetLogicalUnit(lun).GetUnitDesc().bLUWriteProtect = LUWriteProtect::kNoWriteProtect;
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ASSERT_FALSE(dut_->GetDeviceManager().IsLogicalLunPowerOnWriteProtect());
}

TEST_F(InitTest, CancelIrqOnInitFailure) {
  // Inject LinkStartUp failure so InitController() fails midway after registering IRQ handler
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeLinkStartUp,
      [](ufs_mock_device::UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2,
         uint32_t ucmdarg3) {});

  TestUfs::SetMockDevice(&mock_device_);

  zx::result result = driver_test().StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    args.config(fake_config.ToVmo());
  });
  ASSERT_TRUE(result.is_error());
}

TEST_F(InitTest, DispatcherShutdownSynchronization) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  // Post tasks to the driver's worker dispatchers right before driver stop.
  // The worker dispatchers must execute these tasks and flush cleanly prior to/during shutdown.
  libsync::Completion io_done;
  async::PostTask(dut_->io_worker_dispatcher()->async_dispatcher(),
                  [&io_done] { io_done.Signal(); });

  libsync::Completion admin_done;
  async::PostTask(dut_->admin_worker_dispatcher()->async_dispatcher(),
                  [&admin_done] { admin_done.Signal(); });

  // Wait for posted tasks on worker dispatchers to execute cleanly (up to 60s).
  ASSERT_OK(io_done.Wait(zx::sec(60)));
  ASSERT_OK(admin_done.Wait(zx::sec(60)));
}

TEST_F(InitTest, AutoHibernateEnabled) {
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(true)
      .WriteTo(mock_device_.GetRegisters());
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto ahit_reg = AutoHibernateIdleTimerReg::Get().ReadFrom(&dut_->GetMmio());
  EXPECT_EQ(ahit_reg.timer_scale(), AutoHibernateIdleTimerReg::Scale::k1us);
  EXPECT_EQ(ahit_reg.timer_value(), 1000u);
}

TEST_F(InitTest, AutoHibernateDisabledByConfig) {
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(true)
      .WriteTo(mock_device_.GetRegisters());

  TestUfs::SetMockDevice(&mock_device_);

  zx::result result = driver_test().StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    fake_config.enable_auto_hibernate() = false;
    fake_config.auto_hibernate_timer_us() = 1000;
    args.config(fake_config.ToVmo());
  });
  ASSERT_OK(result);
  dut_ = driver_test().driver();

  auto ahit_reg = AutoHibernateIdleTimerReg::Get().ReadFrom(&dut_->GetMmio());
  EXPECT_EQ(ahit_reg.timer_value(), 0u);
}

TEST_F(InitTest, AutoHibernateCustomTimerConfig) {
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(true)
      .WriteTo(mock_device_.GetRegisters());

  TestUfs::SetMockDevice(&mock_device_);

  zx::result result = driver_test().StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    fake_config.enable_auto_hibernate() = true;
    fake_config.auto_hibernate_timer_us() = 50000;
    args.config(fake_config.ToVmo());
  });
  ASSERT_OK(result);
  dut_ = driver_test().driver();

  auto ahit_reg = AutoHibernateIdleTimerReg::Get().ReadFrom(&dut_->GetMmio());
  EXPECT_EQ(ahit_reg.timer_scale(), AutoHibernateIdleTimerReg::Scale::k100us);
  EXPECT_EQ(ahit_reg.timer_value(), 500u);
}

TEST_F(InitTest, AutoHibernateNotSupported) {
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(false)
      .WriteTo(mock_device_.GetRegisters());
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto ahit_reg = AutoHibernateIdleTimerReg::Get().ReadFrom(&dut_->GetMmio());
  EXPECT_EQ(ahit_reg.timer_value(), 0u);
}

TEST_F(InitTest, AutoHibernateQemuQuirk) {
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(true)
      .WriteTo(mock_device_.GetRegisters());
  driver_test().RunInEnvironmentTypeContext([](Environment& env) {
    constexpr uint16_t kRedHatVendorId = 0x1b36;
    constexpr uint16_t kQemuUfsHostController = 0x0013;
    env.pci_server().SetDeviceInfo({
        .vendor_id = kRedHatVendorId,
        .device_id = kQemuUfsHostController,
    });
  });
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  auto ahit_reg = AutoHibernateIdleTimerReg::Get().ReadFrom(&dut_->GetMmio());
  EXPECT_EQ(ahit_reg.timer_value(), 0u);
}

TEST_F(InitTest, CommandTimeoutConfig) {
  TestUfs::SetMockDevice(&mock_device_);

  zx::result result = driver_test().StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    fake_config.enable_auto_hibernate() = false;
    fake_config.command_timeout_sec() = 15;
    args.config(fake_config.ToVmo());
  });
  ASSERT_OK(result);
  dut_ = driver_test().driver();

  EXPECT_EQ(dut_->GetTransferRequestProcessor().GetTimeout(), zx::sec(15));
  EXPECT_EQ(dut_->GetTaskManagementRequestProcessor().GetTimeout(), zx::sec(15));
}

TEST_F(InitTest, UniproTimingQuirksSamsungDevice) {
  mock_device_.GetDeviceDesc().wManufacturerID = htobe16(kSamsungManufacturerId);
  auto& uic = mock_device_.GetUicCmdProcessor();

  // Both host and peer start at 200 us (2 * 100 us); the quirk must raise peer PA_TActivate above
  // host PA_TActivate and add 1 unit of PA_Hibern8Time margin.
  uic.SetMibValue(PA_TActivate, 2);
  uic.SetMibValue(PA_Granularity, 6);
  uic.SetMibValue(PA_Hibern8Time, 1);
  uic.SetMibValue(PA_TActivate, 2, /*peer=*/true);
  uic.SetMibValue(PA_Granularity, 6, /*peer=*/true);

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  using DmeSetRecord = ufs_mock_device::DmeSetRecord;
  EXPECT_EQ(uic.GetDmeSetLog({PA_TActivate, PA_Hibern8Time}),
            (std::vector<DmeSetRecord>{
                {.key = {.attribute = PA_TActivate, .peer = true}, .value = 3},
                {.key = {.attribute = PA_Hibern8Time, .peer = false}, .value = 2},
            }));
  EXPECT_EQ(uic.GetDmeGetCount(PA_Hibern8Time), 1u);

  // Repeated RestoreDeviceAndLinkConfig() calls must set PA_Hibern8Time idempotently to 2 (from
  // the cached initial baseline) without re-reading or incrementing it further.
  uic.SetMibValue(PA_TActivate, 2, /*peer=*/true);
  ASSERT_OK(dut_->GetDeviceManager().RestoreDeviceAndLinkConfig());
  EXPECT_EQ(uic.GetDmeSetLog({PA_TActivate, PA_Hibern8Time}),
            (std::vector<DmeSetRecord>{
                {.key = {.attribute = PA_TActivate, .peer = true}, .value = 3},
                {.key = {.attribute = PA_Hibern8Time, .peer = false}, .value = 2},
                {.key = {.attribute = PA_TActivate, .peer = true}, .value = 3},
                {.key = {.attribute = PA_Hibern8Time, .peer = false}, .value = 2},
            }));
  EXPECT_EQ(uic.GetDmeGetCount(PA_Hibern8Time), 1u);

  // A failing quirk DME_PEER_SET fails the restore (so resume retries) instead of continuing with
  // a link that lacks the Hibern8 exit margin; PA_Hibern8Time is not touched afterwards.
  uic.SetMibValue(PA_TActivate, 2, /*peer=*/true);
  uic.FailDmeSet(PA_TActivate, /*peer=*/true);
  EXPECT_TRUE(dut_->GetDeviceManager().RestoreDeviceAndLinkConfig().is_error());
  EXPECT_EQ(uic.GetDmeSetLog({PA_TActivate, PA_Hibern8Time}).size(), 4u);

  // Inspect still reports the values from the last successful configuration.
  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* unipro = hierarchy.value().GetByPath({"ufs", "controller", "unipro"});
  ASSERT_NE(unipro, nullptr);
  EXPECT_EQ(unipro->node().get_property<inspect::UintPropertyValue>("host_t_activate")->value(),
            2u);
  EXPECT_EQ(unipro->node().get_property<inspect::UintPropertyValue>("device_t_activate")->value(),
            3u);
  EXPECT_EQ(unipro->node().get_property<inspect::UintPropertyValue>("host_hibern8_time")->value(),
            2u);
  EXPECT_TRUE(
      unipro->node().get_property<inspect::BoolPropertyValue>("peer_pa_tactivate_quirk")->value());
  EXPECT_TRUE(unipro->node()
                  .get_property<inspect::BoolPropertyValue>("host_pa_hibern8time_quirk")
                  ->value());
}

TEST_F(InitTest, UniproTimingQuirksMixedGranularityAndUnitAwareComparison) {
  mock_device_.GetDeviceDesc().wManufacturerID = htobe16(kSamsungManufacturerId);
  auto& uic = mock_device_.GetUicCmdProcessor();

  // Host PA_TActivate = 2 with gran 1 (1 us -> 2 us), Peer PA_TActivate = 1 with gran 6
  // (100 us -> 100 us). Raw 2 > 1, but 2 us < 100 us, so peer PA_TActivate must NOT be modified
  // during StartDriver().
  uic.SetMibValue(PA_TActivate, 2);
  uic.SetMibValue(PA_Granularity, 1);
  uic.SetMibValue(PA_Hibern8Time, 4);
  uic.SetMibValue(PA_TActivate, 1, /*peer=*/true);
  uic.SetMibValue(PA_Granularity, 6, /*peer=*/true);

  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_TRUE(uic.GetDmeSets(PA_TActivate, /*peer=*/true).empty());
  EXPECT_EQ(uic.GetDmeSets(PA_Hibern8Time), (std::vector<uint32_t>{5u}));

  // Mixed granularity where host = 128 * 1 us (128 us) > peer = 1 * 100 us (100 us) ->
  // new peer PA_TActivate = (128 / 100) + 1 = 2 (200 us > 128 us).
  uic.SetMibValue(PA_TActivate, 128);
  ASSERT_OK(dut_->GetDeviceManager().RestoreDeviceAndLinkConfig());
  EXPECT_EQ(uic.GetDmeSets(PA_TActivate, /*peer=*/true), (std::vector<uint32_t>{2u}));
  EXPECT_EQ(uic.GetDmeSets(PA_Hibern8Time), (std::vector<uint32_t>{5u, 5u}));
}

TEST_F(InitTest, UniproTimingQuirksGoogleHostAndSamsungDevice) {
  HostControllerProdManufIdReg::Get()
      .FromValue(0)
      .set_manufacturer_id(kGoogleManufacturerId)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.GetDeviceDesc().wManufacturerID = htobe16(kSamsungManufacturerId);
  auto& uic = mock_device_.GetUicCmdProcessor();

  // Host starts at 100 us (1 * 100 us) and peer starts at 200 us (2 * 100 us). The Google host
  // quirk must increment local PA_TActivate to 200 us first so the Samsung device quirk then
  // observes host == peer and raises peer PA_TActivate to 300 us (3 * 100 us).
  uic.SetMibValue(PA_TActivate, 1);
  uic.SetMibValue(PA_Granularity, 6);
  uic.SetMibValue(PA_Hibern8Time, 1);
  uic.SetMibValue(PA_TActivate, 2, /*peer=*/true);
  uic.SetMibValue(PA_Granularity, 6, /*peer=*/true);

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  using DmeSetRecord = ufs_mock_device::DmeSetRecord;
  EXPECT_EQ(uic.GetDmeSetLog({PA_TActivate, PA_Hibern8Time}),
            (std::vector<DmeSetRecord>{
                {.key = {.attribute = PA_TActivate, .peer = false}, .value = 2},
                {.key = {.attribute = PA_TActivate, .peer = true}, .value = 3},
                {.key = {.attribute = PA_Hibern8Time, .peer = false}, .value = 2},
            }));

  // RestoreDeviceAndLinkConfig() must restore local PA_TActivate idempotently to 2 even if the
  // mock retains the previous value, and only adjust peer PA_TActivate when needed.
  uic.SetMibValue(PA_TActivate, 2, /*peer=*/true);
  ASSERT_OK(dut_->GetDeviceManager().RestoreDeviceAndLinkConfig());
  EXPECT_EQ(uic.GetDmeSetLog({PA_TActivate, PA_Hibern8Time}),
            (std::vector<DmeSetRecord>{
                {.key = {.attribute = PA_TActivate, .peer = false}, .value = 2},
                {.key = {.attribute = PA_TActivate, .peer = true}, .value = 3},
                {.key = {.attribute = PA_Hibern8Time, .peer = false}, .value = 2},
                {.key = {.attribute = PA_TActivate, .peer = false}, .value = 2},
                {.key = {.attribute = PA_TActivate, .peer = true}, .value = 3},
                {.key = {.attribute = PA_Hibern8Time, .peer = false}, .value = 2},
            }));

  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* unipro = hierarchy.value().GetByPath({"ufs", "controller", "unipro"});
  ASSERT_NE(unipro, nullptr);
  EXPECT_EQ(unipro->node().get_property<inspect::UintPropertyValue>("host_t_activate")->value(),
            2u);
  EXPECT_EQ(unipro->node().get_property<inspect::UintPropertyValue>("device_t_activate")->value(),
            3u);
}

namespace {

// Starts the driver with suspend disabled and returns the start result so tests can check that a
// timing-quirk DME failure aborts initialization.
template <typename DriverTest>
auto StartDriverExpectingResult(ufs_mock_device::UfsMockDevice& mock_device,
                                DriverTest& driver_test) {
  TestUfs::SetMockDevice(&mock_device);
  return driver_test.StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
    ufs_config::Config fake_config;
    fake_config.enable_suspend() = false;
    fake_config.storage_power_management_enabled() = false;
    args.config(fake_config.ToVmo());
  });
}

}  // namespace

TEST_F(InitTest, UniproTimingQuirksGoogleHostOnly) {
  HostControllerProdManufIdReg::Get()
      .FromValue(0)
      .set_manufacturer_id(kGoogleManufacturerId)
      .WriteTo(mock_device_.GetRegisters());
  auto& uic = mock_device_.GetUicCmdProcessor();
  uic.SetMibValue(PA_TActivate, kTActivate);

  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_EQ(uic.GetDmeSets(PA_TActivate), (std::vector<uint32_t>{kTActivate + 1}));

  // The mock keeps the incremented value, as hardware would without a PowerDown reset; the restore
  // must still program baseline + 1 rather than compounding the increment.
  ASSERT_OK(dut_->GetDeviceManager().RestoreDeviceAndLinkConfig());
  EXPECT_EQ(uic.GetDmeSets(PA_TActivate), (std::vector<uint32_t>{kTActivate + 1, kTActivate + 1}));
  EXPECT_TRUE(uic.GetDmeSets(PA_TActivate, /*peer=*/true).empty());
  EXPECT_TRUE(uic.GetDmeSets(PA_Hibern8Time).empty());

  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* unipro = hierarchy.value().GetByPath({"ufs", "controller", "unipro"});
  ASSERT_NE(unipro, nullptr);
  EXPECT_TRUE(
      unipro->node().get_property<inspect::BoolPropertyValue>("host_pa_tactivate_quirk")->value());
  EXPECT_FALSE(
      unipro->node().get_property<inspect::BoolPropertyValue>("peer_pa_tactivate_quirk")->value());
  EXPECT_FALSE(unipro->node()
                   .get_property<inspect::BoolPropertyValue>("host_pa_hibern8time_quirk")
                   ->value());
  EXPECT_EQ(unipro->node().get_property<inspect::UintPropertyValue>("host_t_activate")->value(),
            kTActivate + 1);
}

TEST_F(InitTest, UniproTimingQuirksInvalidGranularitySkipsPeerTuning) {
  mock_device_.GetDeviceDesc().wManufacturerID = htobe16(kSamsungManufacturerId);
  auto& uic = mock_device_.GetUicCmdProcessor();
  // PA_Granularity 7 is outside the UniPro 1..6 encoding, so the peer margin cannot be computed.
  uic.SetMibValue(PA_Granularity, 7, /*peer=*/true);

  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_TRUE(uic.GetDmeSets(PA_TActivate, /*peer=*/true).empty());
  EXPECT_EQ(uic.GetDmeSets(PA_Hibern8Time), (std::vector<uint32_t>{kHibern8Time + 1}));
}

TEST_F(InitTest, UniproTimingQuirksHostTActivateSetFailureFailsInit) {
  HostControllerProdManufIdReg::Get()
      .FromValue(0)
      .set_manufacturer_id(kGoogleManufacturerId)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.GetUicCmdProcessor().FailDmeSet(PA_TActivate);

  EXPECT_TRUE(StartDriverExpectingResult(mock_device_, driver_test()).is_error());
}

TEST_F(InitTest, UniproTimingQuirksHibern8TimeSetFailureFailsInit) {
  mock_device_.GetDeviceDesc().wManufacturerID = htobe16(kSamsungManufacturerId);
  mock_device_.GetUicCmdProcessor().FailDmeSet(PA_Hibern8Time);

  EXPECT_TRUE(StartDriverExpectingResult(mock_device_, driver_test()).is_error());
}

TEST_F(InitTest, PendingInterruptAtStartupDoesNotCrashIsr) {
  // Leave UE, UTRCS, and UTMRCS latched and the virtual IRQ already pending before Start(),
  // matching a stale interrupt left over across a driver restart.
  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_uic_error(true)
      .set_utp_transfer_request_completion_status(true)
      .set_utp_task_management_request_completion_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();

  auto utmrc_cleared = std::make_shared<libsync::Completion>();
  auto kinit_wait_status = std::make_shared<std::atomic<zx_status_t>>(ZX_OK);
  auto pre_link_wait_status = std::make_shared<std::atomic<zx_status_t>>(ZX_ERR_TIMED_OUT);
  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kIS, [utmrc_cleared](UfsMockDevice& mock_device, uint32_t value) {
        RegisterMmioProcessor::DefaultISHandler(mock_device, value);
        if (InterruptStatusReg::Get()
                .FromValue(value)
                .utp_task_management_request_completion_status()) {
          utmrc_cleared->Signal();
        }
      });

  // Pause briefly in NotifyEvent::kInit, which runs before the request processors are created. If
  // irq_handler_.Begin() ran earlier, the pending IRQ invokes Isr() while both processors are null.
  // Wait for the stale ISR to finish clearing kIS in NotifyEvent::kPreLinkStartup so it does not
  // race with SendLinkStartUp()'s unlocked FakeRegisters kIS read-modify-writes.
  TestUfs::SetStartupHostControllerCallback(
      [utmrc_cleared, kinit_wait_status, pre_link_wait_status](NotifyEvent event,
                                                               uint64_t data) -> zx::result<> {
        if (event == NotifyEvent::kInit) {
          kinit_wait_status->store(utmrc_cleared->Wait(zx::msec(50)));
        } else if (event == NotifyEvent::kPreLinkStartup) {
          pre_link_wait_status->store(utmrc_cleared->Wait(zx::sec(5)));
        }
        return Ufs::NotifyEventCallback(event, data);
      });
  auto clear_callback = fit::defer([this] {
    TestUfs::SetStartupHostControllerCallback(nullptr);
    if (dut_ != nullptr) {
      dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
    }
  });

  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_EQ(kinit_wait_status->load(), ZX_ERR_TIMED_OUT);
  EXPECT_OK(pre_link_wait_status->load());
  EXPECT_FALSE(InterruptStatusReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .utp_task_management_request_completion_status());

  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* ufs_node = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs_node, nullptr);
  EXPECT_EQ(ufs_node->node().get_property<inspect::UintPropertyValue>("uic_error_count")->value(),
            1u);
}

TEST_F(InitTest, FailedInitControllerWithPendingInterruptDoesNotCrash) {
  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_utp_transfer_request_completion_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();

  auto is_read = std::make_shared<libsync::Completion>();
  mock_device_.GetRegisterMmioProcessor().SetReadHook(
      RegisterMap::kIS, [is_read](UfsMockDevice& mock_device) {
        is_read->Signal();
        return mock_device.GetRegisters()->Read<uint32_t>(RegisterMap::kIS);
      });

  TestUfs::SetStartupHostControllerCallback(
      [is_read](NotifyEvent event, uint64_t data) -> zx::result<> {
        if (event == NotifyEvent::kInit) {
          static_cast<void>(is_read->Wait(zx::msec(50)));
          return zx::error(ZX_ERR_IO);
        }
        return Ufs::NotifyEventCallback(event, data);
      });
  auto clear_callback = fit::defer([] { TestUfs::SetStartupHostControllerCallback(nullptr); });

  zx::result<> result = StartDriverExpectingResult(mock_device_, driver_test());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_IO);
  EXPECT_FALSE(is_read->signaled());
}

TEST_F(InitTest, RejectedPowerModeReadsDiagnosticMibsAndDumpsErrorRegisters) {
  // UFSHCI 3.0 section 5.6.3 / MIPI UniPro ConfigResultCode: INVALID_MIB_ATTRIBUTE_VALUE.
  constexpr uint8_t kInvalidMibAttributeValue = 2;

  auto& uic = mock_device_.GetUicCmdProcessor();
  std::atomic<uint32_t> uecn_reads{0};
  std::atomic<bool> dumped_before_diag_mibs{false};
  auto reset_hooks = fit::defer([this] { StopDriverAndResetHooks(); });

  uic.SetHook(UicCommandOpcode::kDmeSet, [](UfsMockDevice& mock_device, uint32_t arg1,
                                            uint32_t arg2, uint32_t arg3) {
    const uint16_t attr =
        static_cast<uint16_t>(UicCommandArgument1Reg::Get().FromValue(arg1).mib_attribute());
    if (attr == PA_PWRMode) {
      UicCommandArgument2Reg::Get()
          .ReadFrom(mock_device.GetRegisters())
          .set_result_code(
              static_cast<UicCommandArgument2Reg::GenericErrorCode>(kInvalidMibAttributeValue))
          .WriteTo(mock_device.GetRegisters());
      return;
    }
    UicCmdProcessor::DefaultDmeSetHandler(mock_device, arg1, arg2, arg3);
  });

  mock_device_.GetRegisterMmioProcessor().SetReadHook(
      RegisterMap::kUECN,
      [&uecn_reads, &uic, &dumped_before_diag_mibs](UfsMockDevice& mock_device) {
        if (uecn_reads.fetch_add(1) == 0 && uic.GetDmeGetCount(PA_ConnectedTxDataLanes) == 0) {
          dumped_before_diag_mibs.store(true);
        }
        return mock_device.GetRegisters()->Read<uint32_t>(RegisterMap::kUECN);
      });

  zx::result<> result = StartDriverExpectingResult(mock_device_, driver_test());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_INTERNAL);

  EXPECT_EQ(uic.GetDmeGetCount(PA_ConnectedTxDataLanes), 1u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_ConnectedRxDataLanes), 1u);
  // Each attribute is queried once during power-mode setup and once in the rejection diagnostic.
  EXPECT_EQ(uic.GetDmeGetCount(PA_AvailTxDataLanes), 2u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_AvailRxDataLanes), 2u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_MaxRxHSGear), 2u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_MaxRxHSGear, /*peer=*/true), 1u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_PWRMode), 1u);
  EXPECT_EQ(uecn_reads.load(), 1u);
  EXPECT_TRUE(dumped_before_diag_mibs.load());
}

TEST_F(InitTest, RejectedPowerModeDumpsErrorRegistersWhenBestEffortReadFails) {
  constexpr uint8_t kInvalidMibAttributeValue = 2;

  auto& uic = mock_device_.GetUicCmdProcessor();
  std::atomic<uint32_t> uecn_reads{0};
  auto reset_hooks = fit::defer([this] { StopDriverAndResetHooks(); });

  uic.SetHook(UicCommandOpcode::kDmeSet, [](UfsMockDevice& mock_device, uint32_t arg1,
                                            uint32_t arg2, uint32_t arg3) {
    const uint16_t attr =
        static_cast<uint16_t>(UicCommandArgument1Reg::Get().FromValue(arg1).mib_attribute());
    if (attr == PA_PWRMode) {
      UicCommandArgument2Reg::Get()
          .ReadFrom(mock_device.GetRegisters())
          .set_result_code(
              static_cast<UicCommandArgument2Reg::GenericErrorCode>(kInvalidMibAttributeValue))
          .WriteTo(mock_device.GetRegisters());
      return;
    }
    UicCmdProcessor::DefaultDmeSetHandler(mock_device, arg1, arg2, arg3);
  });
  uic.SetHook(UicCommandOpcode::kDmePeerGet, [](UfsMockDevice& mock_device, uint32_t arg1,
                                                uint32_t arg2, uint32_t arg3) {
    const uint16_t attr =
        static_cast<uint16_t>(UicCommandArgument1Reg::Get().FromValue(arg1).mib_attribute());
    if (attr == PA_MaxRxHSGear) {
      UicCommandArgument2Reg::Get()
          .ReadFrom(mock_device.GetRegisters())
          .set_result_code(UicCommandArgument2Reg::GenericErrorCode::kFailure)
          .WriteTo(mock_device.GetRegisters());
      return;
    }
    UicCmdProcessor::DefaultDmePeerGetHandler(mock_device, arg1, arg2, arg3);
  });

  mock_device_.GetRegisterMmioProcessor().SetReadHook(
      RegisterMap::kUECN, [&uecn_reads](UfsMockDevice& mock_device) {
        uecn_reads.fetch_add(1);
        return mock_device.GetRegisters()->Read<uint32_t>(RegisterMap::kUECN);
      });

  zx::result<> result = StartDriverExpectingResult(mock_device_, driver_test());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_INTERNAL);

  EXPECT_EQ(uic.GetDmeGetCount(PA_PWRMode), 1u);
  EXPECT_EQ(uecn_reads.load(), 1u);
}

TEST_F(InitTest, ClearsUecpaAndUecdlAfterLinkStartup) {
  const uint32_t stale_uecpa = HostUicErrorCodePhyAdapterLayerReg::Get()
                                   .FromValue(0)
                                   .set_uic_phy_adapter_layer_error(true)
                                   .set_uic_phy_adapter_layer_error_code(0x10)
                                   .reg_value();
  const uint32_t stale_uecdl = HostUicErrorCodeDataLinkLayerReg::Get()
                                   .FromValue(0)
                                   .set_uic_data_link_layer_error(true)
                                   .set_uic_data_link_layer_error_code(0x01)
                                   .reg_value();

  std::atomic<uint32_t> uecpa_reads{0};
  std::atomic<uint32_t> uecdl_reads{0};
  std::atomic<bool> cleared_before_is_ue{false};
  auto reset_hooks = fit::defer([this] { StopDriverAndResetHooks(); });

  auto install_read_to_clear = [&](RegisterMap reg, uint32_t initial_val,
                                   std::atomic<uint32_t>& counter) {
    mock_device_.GetRegisters()->Write<uint32_t>(initial_val, reg);
    mock_device_.GetRegisterMmioProcessor().SetReadHook(
        reg, [&counter, reg](UfsMockDevice& mock_device) {
          counter.fetch_add(1);
          const uint32_t val = mock_device.GetRegisters()->Read<uint32_t>(reg);
          mock_device.GetRegisters()->Write<uint32_t>(0, reg);
          return val;
        });
  };
  install_read_to_clear(RegisterMap::kUECPA, stale_uecpa, uecpa_reads);
  install_read_to_clear(RegisterMap::kUECDL, stale_uecdl, uecdl_reads);

  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kIS, [&](UfsMockDevice& mock_device, uint32_t value) {
        if (InterruptStatusReg::Get().FromValue(value).uic_error() && uecpa_reads.load() > 0 &&
            uecdl_reads.load() > 0) {
          cleared_before_is_ue.store(true);
        }
        RegisterMmioProcessor::DefaultISHandler(mock_device, value);
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver());

  EXPECT_EQ(uecpa_reads.load(), 1u);
  EXPECT_EQ(uecdl_reads.load(), 1u);
  EXPECT_TRUE(cleared_before_is_ue.load());
  EXPECT_EQ(mock_device_.GetRegisters()->Read<uint32_t>(RegisterMap::kUECPA), 0u);
  EXPECT_EQ(mock_device_.GetRegisters()->Read<uint32_t>(RegisterMap::kUECDL), 0u);
}

TEST_F(InitTest, RejectedPowerModeSkipsDiagnosticMibsOnTimeout) {
  TestUfs::SetUicCommandTimeoutUsec(1000);
  auto& uic = mock_device_.GetUicCmdProcessor();
  std::atomic<uint32_t> uecn_reads{0};
  auto reset_hooks = fit::defer([this] { StopDriverAndResetHooks(); });

  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kUICCMD, [](UfsMockDevice& mock_device, uint32_t value) {
        const auto opcode =
            static_cast<UicCommandOpcode>(UicCommandReg::Get().FromValue(value).command_opcode());
        const uint16_t attr = static_cast<uint16_t>(
            UicCommandArgument1Reg::Get().ReadFrom(mock_device.GetRegisters()).mib_attribute());
        if (opcode == UicCommandOpcode::kDmeSet && attr == PA_PWRMode) {
          // Omit IS.UCCS so DME_SET(PA_PWRMode) fails with ZX_ERR_TIMED_OUT.
          UicCommandReg::Get().FromValue(value).WriteTo(mock_device.GetRegisters());
          return;
        }
        RegisterMmioProcessor::DefaultUICCMDHandler(mock_device, value);
      });

  mock_device_.GetRegisterMmioProcessor().SetReadHook(
      RegisterMap::kUECN, [&uecn_reads](UfsMockDevice& mock_device) {
        uecn_reads.fetch_add(1);
        return mock_device.GetRegisters()->Read<uint32_t>(RegisterMap::kUECN);
      });

  zx::result<> result = StartDriverExpectingResult(mock_device_, driver_test());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_TIMED_OUT);

  // None of the 7 diagnostic MIB reads should run after a UIC timeout.
  EXPECT_EQ(uic.GetDmeGetCount(PA_ConnectedTxDataLanes), 0u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_ConnectedRxDataLanes), 0u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_AvailTxDataLanes), 1u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_AvailRxDataLanes), 1u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_MaxRxHSGear), 1u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_MaxRxHSGear, /*peer=*/true), 0u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_PWRMode), 0u);
  EXPECT_EQ(uecn_reads.load(), 1u);
}

TEST_F(InitTest, RejectedPowerModeStopsDiagnosticReadsAfterFirstTimeout) {
  constexpr uint8_t kInvalidMibAttributeValue = 2;
  TestUfs::SetUicCommandTimeoutUsec(1000);

  auto& uic = mock_device_.GetUicCmdProcessor();
  std::atomic<uint32_t> peer_gear_attempts{0};
  std::atomic<uint32_t> uecn_reads{0};
  auto reset_hooks = fit::defer([this] { StopDriverAndResetHooks(); });

  uic.SetHook(UicCommandOpcode::kDmeSet, [](UfsMockDevice& mock_device, uint32_t arg1,
                                            uint32_t arg2, uint32_t arg3) {
    const uint16_t attr =
        static_cast<uint16_t>(UicCommandArgument1Reg::Get().FromValue(arg1).mib_attribute());
    if (attr == PA_PWRMode) {
      UicCommandArgument2Reg::Get()
          .ReadFrom(mock_device.GetRegisters())
          .set_result_code(
              static_cast<UicCommandArgument2Reg::GenericErrorCode>(kInvalidMibAttributeValue))
          .WriteTo(mock_device.GetRegisters());
      return;
    }
    UicCmdProcessor::DefaultDmeSetHandler(mock_device, arg1, arg2, arg3);
  });

  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kUICCMD, [&peer_gear_attempts](UfsMockDevice& mock_device, uint32_t value) {
        const auto opcode =
            static_cast<UicCommandOpcode>(UicCommandReg::Get().FromValue(value).command_opcode());
        const uint16_t attr = static_cast<uint16_t>(
            UicCommandArgument1Reg::Get().ReadFrom(mock_device.GetRegisters()).mib_attribute());
        if (opcode == UicCommandOpcode::kDmePeerGet && attr == PA_MaxRxHSGear) {
          peer_gear_attempts.fetch_add(1);
          UicCommandReg::Get().FromValue(value).WriteTo(mock_device.GetRegisters());
          return;
        }
        RegisterMmioProcessor::DefaultUICCMDHandler(mock_device, value);
      });

  mock_device_.GetRegisterMmioProcessor().SetReadHook(
      RegisterMap::kUECN, [&uecn_reads](UfsMockDevice& mock_device) {
        uecn_reads.fetch_add(1);
        return mock_device.GetRegisters()->Read<uint32_t>(RegisterMap::kUECN);
      });

  zx::result<> result = StartDriverExpectingResult(mock_device_, driver_test());
  ASSERT_TRUE(result.is_error());
  EXPECT_EQ(result.status_value(), ZX_ERR_INTERNAL);

  EXPECT_EQ(uic.GetDmeGetCount(PA_ConnectedTxDataLanes), 1u);
  EXPECT_EQ(uic.GetDmeGetCount(PA_ConnectedRxDataLanes), 1u);
  EXPECT_EQ(peer_gear_attempts.load(), 1u);
  // PA_PWRMode comes after peer PA_MaxRxHSGear and must be skipped once a read times out.
  EXPECT_EQ(uic.GetDmeGetCount(PA_PWRMode), 0u);
  EXPECT_EQ(uecn_reads.load(), 1u);
}

}  // namespace ufs
