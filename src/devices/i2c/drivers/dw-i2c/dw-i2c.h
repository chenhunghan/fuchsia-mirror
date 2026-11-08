// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_I2C_DRIVERS_DW_I2C_DW_I2C_H_
#define SRC_DEVICES_I2C_DRIVERS_DW_I2C_DW_I2C_H_

#include <fidl/fuchsia.hardware.clock/cpp/wire.h>
#include <fidl/fuchsia.hardware.i2c.businfo/cpp/fidl.h>
#include <fidl/fuchsia.hardware.i2cimpl/cpp/driver/wire.h>
#include <fidl/fuchsia.hardware.pin/cpp/wire.h>
#include <fidl/fuchsia.hardware.powerdomain/cpp/wire.h>
#include <fidl/fuchsia.hardware.reset/cpp/wire.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/health_alert_latch.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/driver/mmio/cpp/mmio-buffer.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/zx/event.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/port.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>

#include <memory>
#include <mutex>
#include <span>
#include <string_view>
#include <variant>
#include <vector>

#include "dw-i2c-regs.h"

namespace dw_i2c {

class DwI2c : public fdf::DriverBase2, public fdf::WireServer<fuchsia_hardware_i2cimpl::Device> {
 public:
  static constexpr std::string_view kDriverName = "dw-i2c";
  static constexpr std::string_view kChildNodeName = "dw-i2c";
  static constexpr uint32_t kDwCompTypeNum = 0x44570140;

  // Health condition reason codes.
  static constexpr std::string_view kReasonEnableTimeout = "I2C_ENABLE_TIMEOUT";
  static constexpr std::string_view kReasonDisableTimeout = "I2C_DISABLE_TIMEOUT";
  static constexpr std::string_view kReasonBusBusyTimeout = "I2C_BUS_BUSY_TIMEOUT";
  static constexpr std::string_view kReasonTransactionTimeout = "I2C_TRANSACTION_TIMEOUT";
  static constexpr std::string_view kReasonSclStuckLow = "I2C_SCL_STUCK_LOW";
  static constexpr std::string_view kReasonUnclockedMmioRead = "I2C_UNCLOCKED_MMIO_READ";
  static constexpr std::string_view kReasonResumeFailed = "I2C_RESUME_FAILED";

  // Health diagnostic keys.
  static constexpr std::string_view kDiagResourcesEnabled = "resources_enabled";
  static constexpr std::string_view kDiagTimeoutCount = "timeout_count";
  static constexpr std::string_view kDiagTxAbortCount = "tx_abort_count";
  static constexpr std::string_view kDiagLastAbortSource = "last_abort_source";
  static constexpr std::string_view kDiagResumeFailureCount = "resume_failure_count";
  static constexpr std::string_view kDiagUnclockedReadCount = "unclocked_read_count";
  static constexpr std::string_view kDiagCompType = "comp_type";
  static constexpr std::string_view kDiagExpectedCompType = "expected_comp_type";
  static constexpr std::string_view kDiagEnableStatus = "enable_status";
  static constexpr std::string_view kDiagStatus = "status";
  static constexpr std::string_view kDiagRawIntrStat = "raw_intr_stat";
  static constexpr std::string_view kDiagTxAbrtSource = "tx_abrt_source";
  static constexpr std::string_view kDiagReason = "reason";

  explicit DwI2c() : fdf::DriverBase2(kDriverName) {}

  ~DwI2c() override = default;

  // fdf::DriverBase2 implementation.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;
  void SystemSuspend(fdf::SuspendCompleter completer) override;
  void SystemResume(std::optional<fuchsia_power_broker::LeaseToken> pe_lease,
                    fdf::ResumeCompleter completer) override;
  // fdf::DriverBase2 health hook. kPing reports the latched fault set and lifetime counters
  // without touching hardware; kFast additionally samples status registers when the controller is
  // powered.
  void HealthCheck(fuchsia_driver_framework::HealthCheckRequest request,
                   HealthCheckCompleter completer) override;

  // fdf::WireServer<fuchsia_hardware_i2cimpl::Device> implementation.
  void GetMaxTransferSize(fdf::Arena& arena, GetMaxTransferSizeCompleter::Sync& completer) override;
  void SetBitrate(SetBitrateRequestView request, fdf::Arena& arena,
                  SetBitrateCompleter::Sync& completer) override;
  void Transact(TransactRequestView request, fdf::Arena& arena,
                TransactCompleter::Sync& completer) override;
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_i2cimpl::Device> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override;

 protected:
  static constexpr zx::duration kTransferTimeout = zx::sec(1);

  zx::duration transfer_timeout() const { return transfer_timeout_; }
  void set_transfer_timeout(zx::duration timeout) { transfer_timeout_ = timeout; }
  fdf::HealthAlertLatch& health_latch() { return health_latch_; }

 private:
  bool HasLatchedCondition(std::string_view reason_code) const;
  std::vector<fuchsia_driver_framework::HealthDiagnosticItem> BuildCommonDiagnostics() const;
  void RecordTimeout(std::string_view reason);
  void RaiseSclStuckLow(uint32_t raw_intr_stat);

  struct ReadOp {
    std::span<uint8_t> data;
    bool stop;
  };
  struct WriteOp {
    std::span<const uint8_t> data;
    bool stop;
  };
  using Op = std::variant<ReadOp, WriteOp>;

  static constexpr uint32_t kErrorSignal = ZX_USER_SIGNAL_0;
  static constexpr uint32_t kTransactionCompleteSignal = ZX_USER_SIGNAL_1;
  static constexpr uint32_t kI2cDisable = 0;
  static constexpr uint32_t kI2cEnable = 1;
  static constexpr uint32_t kStandardMode = 1;
  static constexpr uint32_t kFastMode = 2;
  static constexpr uint32_t kHighSpeedMode = 3;
  static constexpr uint32_t k7BitAddr = 0;
  static constexpr uint16_t k7BitAddrMask = 0x7f;
  static constexpr uint32_t k10BitAddr = 0;
  static constexpr uint32_t kActive = 1;
  static constexpr uint32_t kMaxPoll = 100;
  static constexpr uint32_t kPollSleep = ZX_USEC(25);
  static constexpr uint32_t kDefaultTimeout = ZX_MSEC(100);
  uint32_t kI2cInterruptReadMask = InterruptMaskReg::Get()
                                       .FromValue(0)
                                       .set_rx_full(1)
                                       .set_tx_abrt(1)
                                       .set_stop_det(1)
                                       .reg_value();
  uint32_t kI2cInterruptDefaultMask = InterruptMaskReg::Get()
                                          .FromValue(0)
                                          .set_rx_full(1)
                                          .set_tx_abrt(1)
                                          .set_stop_det(1)
                                          .set_tx_empty(1)
                                          .reg_value();

  /* I2C timing parameters */
  static constexpr uint32_t kClkRateKHz = 100000;
  static constexpr uint32_t kSclTFalling = 205;
  static constexpr uint32_t kSdaTFalling = 425;
  static constexpr uint32_t kSdaTHold = 449;
  /* Standard speed parameters */
  static constexpr uint32_t kSclStandardSpeedTHold = 4000;  // SCL hold time for start signal in ns
  static constexpr uint32_t kSclStandardSpeedTLow = 4700;   // SCL low time in ns
  /* Fast speed parameters */
  static constexpr uint32_t kSclFastSpeedTHold = 600;  // SCL hold time for start signal in ns
  static constexpr uint32_t kSclFastSpeedTLow = 1300;  // SCL low time in ns

  // IC_[FS]S_SCL_HCNT + 3 >= IC_CLK * (tHD;STA + tf)
  static constexpr uint32_t kSclStandardSpeedHcnt =
      ((kClkRateKHz * (kSclStandardSpeedTHold + kSdaTFalling)) + 500000) / 1000000 - 3;
  static constexpr uint32_t kSclFastSpeedHcnt =
      ((kClkRateKHz * (kSclFastSpeedTHold + kSdaTFalling)) + 500000) / 1000000 - 3;

  // IC_[FS]S_SCL_LCNT + 1 >= IC_CLK * (tLOW + tf)
  static constexpr uint32_t kSclStandardSpeedLcnt =
      ((kClkRateKHz * (kSclStandardSpeedTLow + kSclTFalling)) + 500000) / 1000000 - 1;
  static constexpr uint32_t kSclFastSpeedLcnt =
      ((kClkRateKHz * (kSclFastSpeedTLow + kSclTFalling)) + 500000) / 1000000 - 1;

  // IC_SDA_HOLD = (IC_CLK * tSDA;Hold + 500000 / 1000000)
  static constexpr uint32_t kSdaHoldValue = ((kClkRateKHz * kSdaTHold) + 500000) / 1000000;
  // 100ms in IC_CLK cycles (4x SMBus t_TIMEOUT,MIN = 25ms, well below the 1s kTransferTimeout).
  static constexpr uint32_t kSclStuckLowTimeoutMs = 100;
  static constexpr uint32_t kSclStuckLowTimeoutCycles = kClkRateKHz * kSclStuckLowTimeoutMs;

  // Local buffer for transfer and receive. Matches FIFO size.
  uint32_t kMaxTransfer = 0;

  zx::result<> HostInit();
  zx::result<> Receive();
  zx::result<> Transmit();
  zx::result<> SetSlaveAddress(uint16_t addr);
  void Dumpstate();
  zx::result<> EnableAndWait(bool enable);
  zx::result<> Enable();
  void ClearInterrupts();
  void DisableInterrupts();
  void EnableInterrupts(uint32_t flag);
  zx::result<> Disable();
  InterruptStatusReg ReadAndClearIrq();
  zx::result<> WaitBusBusy();
  void SetOpsHelper(std::vector<Op> ops);
  zx::result<std::vector<fuchsia_hardware_i2cimpl::wire::ReadData>> PrepareOps(
      TransactRequestView request, fdf::Arena& arena);
  void DrainIrqPort(bool& irq_needs_ack);
  zx::result<InterruptStatusReg> WaitForInterrupt(zx::time deadline, bool& irq_needs_ack);
  // Runs the prepared ops on the bus. On success returns whether the controller disabled cleanly
  // afterwards (false means EnableAndWait(false) timed out and I2C_DISABLE_TIMEOUT is latched).
  zx::result<bool> ExecuteTransact();

  virtual zx::result<fdf::MmioBuffer> MapMmio(fdf::PDev& pdev);

  static constexpr uint64_t kIrqPortKey = 1;

  std::optional<fdf::MmioBuffer> mmio_;
  zx::interrupt irq_;
  // Bound to `irq_` so `Transact()` can wait for interrupts with a deadline.
  zx::port irq_port_;
  zx::duration transfer_timeout_ = kTransferTimeout;
  uint32_t tx_fifo_depth_ = 0;
  uint32_t rx_fifo_depth_ = 0;

  std::vector<Op> ops_;
  uint32_t rx_op_idx_ = 0;
  uint32_t tx_op_idx_ = 0;
  uint32_t rx_done_len_ = 0;
  uint32_t tx_done_len_ = 0;
  uint32_t rx_pending_ = 0;
  zx::result<> ServeI2cImpl();
  zx::result<> CreateChildNode();

  fdf::ServerBindingGroup<fuchsia_hardware_i2cimpl::Device> i2cimpl_bindings_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> child_controller_;

  std::optional<fdf::StopCompleter> completer_;
  fdf_metadata::MetadataServer<fuchsia_hardware_i2c_businfo::I2CBusMetadata> metadata_server_;
  bool send_restart_ = false;
  bool resources_enabled_ = false;

  uint64_t timeout_count_ = 0;
  uint64_t tx_abort_count_ = 0;
  uint32_t last_abort_source_ = 0;
  uint64_t resume_failure_count_ = 0;
  uint64_t unclocked_read_count_ = 0;
  fdf::HealthAlertLatch health_latch_{*this};

  fidl::WireSyncClient<fuchsia_hardware_clock::Clock> clock_bus_;
  fidl::WireSyncClient<fuchsia_hardware_clock::Clock> clock_regs_;
  fidl::WireSyncClient<fuchsia_hardware_reset::Reset> reset_;
  fidl::WireSyncClient<fuchsia_hardware_powerdomain::Domain> powerdomain_;
  // Present when the board provides pin states for this controller. The pin configuration (e.g.
  // the IP type selection of a shared I/O block) can be lost when the power domain is turned off,
  // so the default state is selected again on resume.
  std::optional<fidl::WireSyncClient<fuchsia_hardware_pin::PinStates>> pin_states_;
};

}  // namespace dw_i2c

#endif  // SRC_DEVICES_I2C_DRIVERS_DW_I2C_DW_I2C_H_
