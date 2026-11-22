// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-i2c.h"

#include <fidl/fuchsia.hardware.reset/cpp/wire_test_base.h>
#include <lib/driver/fake-clock/cpp/fake-clock.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/fake-powerdomain/cpp/fake-powerdomain.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fit/function.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/zx/clock.h>
#include <lib/zx/interrupt.h>

#include <algorithm>
#include <array>
#include <deque>
#include <initializer_list>
#include <map>
#include <mutex>
#include <optional>
#include <string>
#include <type_traits>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace dw_i2c {

class TestDwI2c : public DwI2c {
 public:
  using DwI2c::health_latch;
  using DwI2c::inspector_for_testing;
  using DwI2c::kAutosuspendDelay;
  using DwI2c::powered;
  using DwI2c::set_autosuspend_delay;
  using DwI2c::set_transfer_timeout;

  // Autosuspend is disabled by default so tests are not sensitive to timing. Tests exercising
  // runtime PM enable it with `set_autosuspend_delay()`.
  explicit TestDwI2c() : DwI2c() { set_autosuspend_delay(zx::duration::infinite()); }

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(fdf_internal::DriverServer2<TestDwI2c>::initialize,
                                          fdf_internal::DriverServer2<TestDwI2c>::destroy);
  }

  static void set_mmio(fdf::MmioBuffer mmio) { mmio_.emplace(std::move(mmio)); }

 protected:
  zx::result<fdf::MmioBuffer> MapMmio(fdf::PDev& pdev) override {
    if (mmio_) {
      return zx::ok(*std::move(mmio_));
    }
    return zx::error(ZX_ERR_BAD_STATE);
  }

 private:
  static std::optional<fdf::MmioBuffer> mmio_;
};

std::optional<fdf::MmioBuffer> TestDwI2c::mmio_;

// Fake DesignWare I2C controller.
//
// Once given the driver's virtual interrupt via `set_interrupt()`, the fake also models the I2C bus
// and the level-triggered interrupt line, synchronously inside the driver's MMIO accesses:
//  - The bus only makes progress when the driver reads a zero InterruptStatusReg while no interrupt
//    is pending, i.e. just before the driver waits for the interrupt. At that point all queued
//    commands complete on the wire (or the transfer aborts, see `AbortNextTransfer()`), and the
//    interrupt is triggered if this asserted an unmasked status bit. The read still returns zero,
//    so the driver must be woken by the interrupt. A consequence of this model is that the driver
//    must re-read InterruptStatusReg after acknowledging the interrupt for the bus to advance.
//  - Whenever an unmasked status bit is asserted while no interrupt is pending, a level-triggered
//    interrupt would fire again. The fake detects this on every register access, re-triggers the
//    interrupt, and counts it in `spurious_retrigger_count()`.
class FakeDwI2cController {
 public:
  static constexpr size_t kRegCount = 0x100;
  static constexpr uint32_t kTxFifoDepth = 8;
  static constexpr uint32_t kRxFifoDepth = 8;

  // Register offsets.
  static constexpr size_t kControl = 0x00;               // ControlReg
  static constexpr size_t kTargetAddress = 0x04;         // TargetAddressReg
  static constexpr size_t kDataCommand = 0x10;           // DataCommandReg
  static constexpr size_t kIntrStat = 0x2c;              // InterruptStatusReg
  static constexpr size_t kIntrMask = 0x30;              // InterruptMaskReg
  static constexpr size_t kRawIntrStat = 0x34;           // RawInterruptStatusReg
  static constexpr size_t kRxTl = 0x38;                  // RxFifoThresholdReg
  static constexpr size_t kTxTl = 0x3c;                  // TxFifoThresholdReg
  static constexpr size_t kClrIntr = 0x40;               // ClearInterruptReg
  static constexpr size_t kClrTxAbrt = 0x54;             // ClearTxAbrtReg
  static constexpr size_t kClrStopDet = 0x60;            // ClearStopDetReg
  static constexpr size_t kEnable = 0x6c;                // EnableReg
  static constexpr size_t kTxFifoLevel = 0x74;           // TxFifoLevelReg
  static constexpr size_t kRxFifoLevel = 0x78;           // RxFifoLevelReg
  static constexpr size_t kTxAbrtSource = 0x80;          // TxAbrtSourceReg
  static constexpr size_t kEnableStatus = 0x9c;          // EnableStatusReg
  static constexpr size_t kSclStuckAtLowTimeout = 0xac;  // SclStuckAtLowTimeoutReg
  static constexpr size_t kClrSclStuckDet = 0xb4;        // ClearSclStuckDetReg
  static constexpr size_t kCompParam1 = 0xf4;            // CompParam1Reg
  static constexpr size_t kCompType = 0xfc;              // CompTypeReg

  struct RecordedCommand {
    uint16_t target_addr;
    uint8_t data;
    bool is_read;
    bool start;
    bool stop;
  };

  FakeDwI2cController() : mmio_(sizeof(uint32_t), kRegCount) {
    for (size_t i = 0; i < kRegCount; ++i) {
      OnRead(i * sizeof(uint32_t), [this, i]() { return regs_[i]; });
      OnWrite(i * sizeof(uint32_t), [this, i](uint32_t val) { regs_[i] = val; });
    }

    // Writing queues TX/read commands; reading pops from RX FIFO.
    OnRead(kDataCommand, [this]() -> uint32_t {
      uint8_t byte = 0;
      if (!rx_fifo_.empty()) {
        byte = rx_fifo_.front();
        rx_fifo_.pop_front();
      }
      UpdateRxFifoStateLocked();
      return byte;
    });
    OnWrite(kDataCommand, [this](uint32_t val) {
      auto cmd = DataCommandReg::Get().FromValue(val);
      auto tar = TargetAddressReg::Get().FromValue(RegLocked(kTargetAddress));
      tx_commands_.push_back(RecordedCommand{
          .target_addr = static_cast<uint16_t>(tar.target_address()),
          .data = static_cast<uint8_t>(cmd.data()),
          .is_read = cmd.command() != 0,
          .start = cmd.start() != 0,
          .stop = cmd.stop() != 0,
      });
      if (tx_fifo_level_ < kTxFifoDepth) {
        ++tx_fifo_level_;
      }
      UpdateTxFifoStateLocked();
      if (cmd.command() != 0) {
        uint8_t rx_byte = 0;
        if (!canned_rx_bytes_.empty()) {
          rx_byte = canned_rx_bytes_.front();
          canned_rx_bytes_.pop_front();
        }
        pending_wire_rx_.push_back(rx_byte);
      }
      if (cmd.stop() != 0) {
        stop_pending_ = true;
      }
    });

    // InterruptStatusReg = RawInterruptStatusReg & InterruptMaskReg.
    OnRead(kIntrStat, [this]() {
      const uint32_t status = InterruptStatusLocked();
      if (status == 0 && irq_.is_valid() && !IsInterruptTriggered()) {
        if (!bus_stalled_) {
          AdvanceBusLocked();
          if (InterruptStatusLocked() != 0) {
            TriggerInterruptLocked();
          }
        } else if (scl_held_low_ &&
                   ControlReg::Get().FromValue(RegLocked(kControl)).bus_clear_feature_ctrl() != 0 &&
                   RegLocked(kSclStuckAtLowTimeout) != 0 &&
                   RegLocked(kSclStuckAtLowTimeout) < 0xffffffffu) {
          auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
          raw.set_scl_stuck_low(1);
          RegLocked(kRawIntrStat) = raw.reg_value();
        }
      }
      return status;
    });

    // The FIFO thresholds determine the raw RX_FULL / TX_EMPTY bits.
    OnWrite(kRxTl, [this](uint32_t val) {
      RegLocked(kRxTl) = val;
      UpdateRxFifoStateLocked();
    });
    OnWrite(kTxTl, [this](uint32_t val) {
      RegLocked(kTxTl) = val;
      UpdateTxFifoStateLocked();
    });

    // Reading the clear registers clears latched raw interrupt status bits.
    const uint32_t tx_abrt = RawInterruptStatusReg::Get().FromValue(0).set_tx_abrt(1).reg_value();
    const uint32_t stop_det = RawInterruptStatusReg::Get().FromValue(0).set_stop_det(1).reg_value();
    const uint32_t scl_stuck_low =
        RawInterruptStatusReg::Get().FromValue(0).set_scl_stuck_low(1).reg_value();
    OnReadClearsRaw(kClrIntr, tx_abrt | stop_det | scl_stuck_low);
    OnReadClearsRaw(kClrTxAbrt, tx_abrt);
    OnReadClearsRaw(kClrSclStuckDet, scl_stuck_low);
    OnRead(kClrStopDet, [this, stop_det]() {
      RegLocked(kRawIntrStat) &= ~stop_det;
      if (retrigger_on_stop_det_clear_) {
        retrigger_on_stop_det_clear_ = false;
        TriggerInterruptLocked();
      }
      return 0u;
    });

    // EnableReg mirrors enable state into EnableStatusReg and flushes FIFOs when disabled, unless
    // the controller has been made to ignore disable requests (see `StickEnabled()`).
    OnWrite(kEnable, [this](uint32_t val) {
      RegLocked(kEnable) = val;
      if ((val & 0x1u) == 0 && stick_enabled_) {
        return;
      }
      RegLocked(kEnableStatus) = val & 0x1u;
      if ((val & 0x1u) == 0) {
        FlushFifosLocked();
      }
      UpdateTxFifoStateLocked();
    });

    // Report FIFO depths of 8 for tx and rx.
    OnRead(kCompParam1, []() { return (kTxFifoDepth << 16) | (kRxFifoDepth << 8); });
    // Report kDwCompTypeNum (0x44570140) when powered, unless overridden.
    OnRead(kCompType, [this]() {
      return powered_ ? comp_type_override_.value_or(DwI2c::kDwCompTypeNum) : 0u;
    });
  }

  fdf::MmioBuffer GetMmioBuffer() { return mmio_.GetMmioBuffer(); }

  uint32_t ReadReg(size_t offset) const {
    std::lock_guard lock(lock_);
    return regs_[offset / sizeof(uint32_t)];
  }

  void ResetWritableRegs() {
    std::lock_guard lock(lock_);
    regs_.fill(0);
    FlushFifosLocked();
  }

  void set_powered(bool powered) {
    std::lock_guard lock(lock_);
    powered_ = powered;
  }

  void set_comp_type_override(std::optional<uint32_t> comp_type) {
    std::lock_guard lock(lock_);
    comp_type_override_ = comp_type;
  }

  // Makes any register access while unpowered fail the test. Touching the registers while the
  // clocks or power domain are off stalls the bus on hardware.
  void set_fail_on_unpowered_access(bool fail) {
    std::lock_guard lock(lock_);
    fail_on_unpowered_access_ = fail;
  }

  // Asserts or clears the SCL_STUCK_AT_LOW raw interrupt status bit so tests can simulate a stuck
  // bus with no transaction in flight.
  void set_scl_stuck_low(bool stuck) {
    std::lock_guard lock(lock_);
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    raw.set_scl_stuck_low(stuck ? 1 : 0);
    RegLocked(kRawIntrStat) = raw.reg_value();
  }

  // Enables modelling of the bus and the level-triggered interrupt line. `irq` must be a duplicate
  // of the virtual interrupt given to the driver, and must be set before the driver starts.
  void set_interrupt(zx::interrupt irq) {
    std::lock_guard lock(lock_);
    irq_ = std::move(irq);
  }

  // Prevents the bus from making progress, so queued transfers never complete. When
  // `scl_stuck_low` is true and Bus Clear is enabled with a finite SCL timeout, asserts
  // `SCL_STUCK_AT_LOW` in `RawInterruptStatusReg` while stalled.
  void StallBus(bool scl_stuck_low = false) {
    std::lock_guard lock(lock_);
    bus_stalled_ = true;
    scl_held_low_ = scl_stuck_low;
  }

  // Lets a previously stalled bus make progress again.
  void UnstallBus() {
    std::lock_guard lock(lock_);
    bus_stalled_ = false;
    scl_held_low_ = false;
  }

  // Makes the controller ignore disable requests: IC_ENABLE_STATUS keeps reporting enabled after
  // the driver writes IC_ENABLE = 0, so the driver's disable wait times out.
  void StickEnabled(bool stick) {
    std::lock_guard lock(lock_);
    stick_enabled_ = stick;
    if (!stick) {
      RegLocked(kEnableStatus) = RegLocked(kEnable) & 0x1u;
    }
  }

  // Makes the next transfer on the bus fail with TX_ABRT, latching `abort_source` in
  // TxAbrtSourceReg (e.g. 0x1 for an address NACK).
  void AbortNextTransfer(uint32_t abort_source) {
    std::lock_guard lock(lock_);
    abort_source_ = abort_source;
  }

  // Triggers the interrupt once more when the driver next clears STOP_DET, i.e. while the driver
  // still has to acknowledge the interrupt that reported it.
  void RetriggerOnNextStopDetClear() {
    std::lock_guard lock(lock_);
    retrigger_on_stop_det_clear_ = true;
  }

  void QueueCannedRxBytes(std::initializer_list<uint8_t> bytes) {
    std::lock_guard lock(lock_);
    canned_rx_bytes_.insert(canned_rx_bytes_.end(), bytes);
  }

  std::vector<RecordedCommand> TakeTxCommands() {
    std::lock_guard lock(lock_);
    return std::exchange(tx_commands_, {});
  }

  // Returns true if the interrupt has been triggered and not yet acknowledged.
  //
  // `irq_` is set once before the driver starts, so this can read it without holding `lock_` (and
  // is also called from MMIO callbacks with `lock_` held).
  bool IsInterruptTriggered() const {
    zx_signals_t observed = 0;
    return irq_.is_valid() && irq_.wait_one(ZX_VIRTUAL_INTERRUPT_UNTRIGGERED,
                                            zx::time::infinite_past(), &observed) != ZX_OK;
  }

  // Number of times the fake triggered the interrupt, excluding spurious re-triggers.
  uint32_t irq_trigger_count() const {
    std::lock_guard lock(lock_);
    return irq_trigger_count_;
  }

  // Number of times an unmasked status bit was asserted while the interrupt wasn't pending (e.g.
  // the driver acknowledged the interrupt too early, or unmasked an already-asserted bit), so the
  // fake re-asserted the level-triggered interrupt.
  uint32_t spurious_retrigger_count() const {
    std::lock_guard lock(lock_);
    return spurious_retrigger_count_;
  }

 private:
  // Installs MMIO callbacks that run with `lock_` held, after checking the access (see
  // `set_fail_on_unpowered_access()`) and re-triggering the interrupt if an unmasked status bit is
  // asserted while the interrupt isn't pending.
  void OnRead(size_t offset, fit::function<uint32_t()> read) {
    mmio_[offset].SetReadCallback([this, offset, read = std::move(read)]() -> uint64_t {
      std::lock_guard lock(lock_);
      CheckAccessLocked(offset);
      RetriggerIfLevelAssertedLocked();
      return read();
    });
  }
  void OnWrite(size_t offset, fit::function<void(uint32_t)> write) {
    mmio_[offset].SetWriteCallback([this, offset, write = std::move(write)](uint64_t val) {
      std::lock_guard lock(lock_);
      CheckAccessLocked(offset);
      RetriggerIfLevelAssertedLocked();
      write(static_cast<uint32_t>(val));
    });
  }
  void CheckAccessLocked(size_t offset) {
    if (!powered_ && fail_on_unpowered_access_) {
      ADD_FAILURE() << "Register 0x" << std::hex << offset << " accessed while unpowered";
    }
  }
  // Installs a read callback that clears `bits` in RawInterruptStatusReg.
  void OnReadClearsRaw(size_t offset, uint32_t bits) {
    OnRead(offset, [this, bits]() {
      RegLocked(kRawIntrStat) &= ~bits;
      return 0u;
    });
  }

  uint32_t& RegLocked(size_t offset) { return regs_[offset / sizeof(uint32_t)]; }

  uint32_t InterruptStatusLocked() { return RegLocked(kRawIntrStat) & RegLocked(kIntrMask); }

  void TriggerInterruptLocked() {
    ++irq_trigger_count_;
    EXPECT_OK(irq_.trigger(0, zx::clock::get_boot()));
  }

  void RetriggerIfLevelAssertedLocked() {
    if (InterruptStatusLocked() != 0 && irq_.is_valid() && !IsInterruptTriggered()) {
      ++spurious_retrigger_count_;
      EXPECT_OK(irq_.trigger(0, zx::clock::get_boot()));
    }
  }

  // Completes (or aborts) all commands queued in the TX FIFO.
  void AdvanceBusLocked() {
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    if (abort_source_) {
      RegLocked(kTxAbrtSource) = *std::exchange(abort_source_, std::nullopt);
      raw.set_tx_abrt(1);
    } else {
      rx_fifo_.insert(rx_fifo_.end(), pending_wire_rx_.begin(), pending_wire_rx_.end());
      if (stop_pending_) {
        raw.set_stop_det(1);
      }
    }
    RegLocked(kRawIntrStat) = raw.reg_value();
    tx_fifo_level_ = 0;
    pending_wire_rx_.clear();
    stop_pending_ = false;
    UpdateTxFifoStateLocked();
    UpdateRxFifoStateLocked();
  }

  void FlushFifosLocked() {
    tx_fifo_level_ = 0;
    pending_wire_rx_.clear();
    stop_pending_ = false;
    rx_fifo_.clear();
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    raw.set_scl_stuck_low(0);
    RegLocked(kRawIntrStat) = raw.reg_value();
    UpdateRxFifoStateLocked();
  }

  void UpdateTxFifoStateLocked() {
    RegLocked(kTxFifoLevel) = tx_fifo_level_;
    const bool enabled = (RegLocked(kEnable) & 0x1u) != 0;
    const uint32_t tx_tl = RegLocked(kTxTl) & 0xffu;
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    raw.set_tx_empty(enabled && tx_fifo_level_ <= tx_tl ? 1 : 0);
    RegLocked(kRawIntrStat) = raw.reg_value();
  }

  void UpdateRxFifoStateLocked() {
    RegLocked(kRxFifoLevel) = static_cast<uint32_t>(rx_fifo_.size());
    const uint32_t rx_tl = RegLocked(kRxTl) & 0xffu;
    auto raw = RawInterruptStatusReg::Get().FromValue(RegLocked(kRawIntrStat));
    raw.set_rx_full(!rx_fifo_.empty() && rx_fifo_.size() > rx_tl ? 1 : 0);
    RegLocked(kRawIntrStat) = raw.reg_value();
  }

  mutable std::mutex lock_;
  bool powered_ = true;
  bool fail_on_unpowered_access_ = false;
  std::optional<uint32_t> comp_type_override_;
  uint32_t tx_fifo_level_ = 0;
  std::array<uint32_t, kRegCount> regs_{};
  std::vector<RecordedCommand> tx_commands_;
  std::deque<uint8_t> canned_rx_bytes_;
  // Bytes to be read by the queued read commands, and whether a queued command has STOP set.
  std::deque<uint8_t> pending_wire_rx_;
  bool stop_pending_ = false;
  std::deque<uint8_t> rx_fifo_;
  zx::interrupt irq_;
  bool bus_stalled_ = false;
  bool scl_held_low_ = false;
  bool stick_enabled_ = false;
  std::optional<uint32_t> abort_source_;
  bool retrigger_on_stop_det_clear_ = false;
  uint32_t irq_trigger_count_ = 0;
  uint32_t spurious_retrigger_count_ = 0;
  fake_mmio::FakeMmioRegRegion mmio_;
};

class FakePinStates : public fidl::WireServer<fuchsia_hardware_pin::PinStates> {
 public:
  fuchsia_hardware_pin::PinStatesService::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_pin::PinStatesService::InstanceHandler({
        .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  void set_on_select_state(fit::function<void(std::string_view)> callback) {
    on_select_state_ = std::move(callback);
  }

  const std::optional<std::string>& last_state() const { return last_state_; }

  // Makes selecting `state` fail with `status`, as a pin controller without that state would.
  void set_select_error(std::string state, zx_status_t status) {
    select_errors_[std::move(state)] = status;
  }
  void clear_select_error(const std::string& state) { select_errors_.erase(state); }
  uint32_t select_count(const std::string& state) const {
    auto it = select_counts_.find(state);
    return it == select_counts_.end() ? 0 : it->second;
  }

 private:
  void SelectState(fuchsia_hardware_pin::wire::PinStatesSelectStateRequest* request,
                   SelectStateCompleter::Sync& completer) override {
    std::string state(request->name.get());
    ++select_counts_[state];
    if (auto it = select_errors_.find(state); it != select_errors_.end()) {
      completer.ReplyError(it->second);
      return;
    }
    if (on_select_state_) {
      on_select_state_(state);
    }
    last_state_ = std::move(state);
    completer.ReplySuccess();
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_pin::PinStates> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  std::optional<std::string> last_state_;
  std::map<std::string, zx_status_t> select_errors_;
  std::map<std::string, uint32_t> select_counts_;
  fit::function<void(std::string_view)> on_select_state_;
  fidl::ServerBindingGroup<fuchsia_hardware_pin::PinStates> bindings_;
};

// The resources whose votes the reference-counted fakes below record.
enum class Resource { kPowerDomain, kIrqController, kRegistersClock, kBusClock };
using enum Resource;

enum class Op { kEnable, kDisable };

// A successful Enable or Disable call on one of the fakes below.
struct Event {
  Resource resource;
  Op op;

  bool operator==(const Event&) const = default;
};

// The successful Enable and Disable calls on the fakes, in order.
using EventLog = std::vector<Event>;

constexpr Event Enabled(Resource resource) { return {resource, Op::kEnable}; }
constexpr Event Disabled(Resource resource) { return {resource, Op::kDisable}; }

void PrintTo(const Event& event, std::ostream* os) {
  constexpr std::array<const char*, 4> kResourceNames = {"power domain", "interrupt controller",
                                                         "registers clock", "bus clock"};
  *os << (event.op == Op::kEnable ? "Enabled(" : "Disabled(")
      << kResourceNames[static_cast<size_t>(event.resource)] << ")";
}

// Reference-counted clock server that only gates the hardware clock on the 0 -> 1 transition and
// off on the 1 -> 0 transition, matching reference-counted platform clock managers.
class RefCountedClock final : public fidl::testing::WireTestBase<fuchsia_hardware_clock::Clock> {
 public:
  fuchsia_hardware_clock::Service::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_clock::Service::InstanceHandler({
        .clock = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  void set_on_hw_gate_change(fit::function<void(bool)> callback) {
    on_hw_gate_change_ = std::move(callback);
  }

  // Simulates forced ambient suspend collapsing the parent power domain and gating off the
  // hardware clock without changing the software reference count in the clock manager.
  void ForceHardwareGateOff() {
    hw_gated_on_ = false;
    if (on_hw_gate_change_) {
      on_hw_gate_change_(false);
    }
  }

  uint32_t ref_count() const { return ref_count_; }
  uint32_t enable_calls() const { return enable_calls_; }
  uint32_t disable_calls() const { return disable_calls_; }
  bool hw_gated_on() const { return hw_gated_on_; }

  void set_enable_result(zx::result<> result) { enable_result_ = result; }
  // Makes Disable fail with `result`, leaving `ref_count_` unchanged.
  void set_disable_result(zx::result<> result) { disable_result_ = result; }
  // Records successful Enable and Disable calls in `log`, as votes on `resource`.
  void set_log(Resource resource, EventLog* log) {
    resource_ = resource;
    log_ = log;
  }

 private:
  void Enable(EnableCompleter::Sync& completer) override {
    ++enable_calls_;
    if (enable_result_.is_error()) {
      completer.ReplyError(enable_result_.status_value());
      return;
    }
    if (log_) {
      log_->push_back(Enabled(resource_));
    }
    if (ref_count_ == 0) {
      hw_gated_on_ = true;
      if (on_hw_gate_change_) {
        on_hw_gate_change_(true);
      }
    }
    ++ref_count_;
    completer.ReplySuccess();
  }

  void Disable(DisableCompleter::Sync& completer) override {
    ++disable_calls_;
    if (disable_result_.is_error()) {
      completer.ReplyError(disable_result_.status_value());
      return;
    }
    if (log_) {
      log_->push_back(Disabled(resource_));
    }
    if (ref_count_ > 0) {
      --ref_count_;
      if (ref_count_ == 0) {
        hw_gated_on_ = false;
        if (on_hw_gate_change_) {
          on_hw_gate_change_(false);
        }
      }
    }
    completer.ReplySuccess();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t ref_count_ = 0;
  uint32_t enable_calls_ = 0;
  uint32_t disable_calls_ = 0;
  bool hw_gated_on_ = false;
  zx::result<> enable_result_ = zx::ok();
  zx::result<> disable_result_ = zx::ok();
  Resource resource_ = kPowerDomain;
  EventLog* log_ = nullptr;
  fit::function<void(bool)> on_hw_gate_change_;
  fidl::ServerBindingGroup<fuchsia_hardware_clock::Clock> bindings_;
};

// Reference-counted power domain server that tracks software enable/disable vote balance across
// driver restarts and suspend/resume cycles.
class RefCountedPowerDomain final
    : public fidl::testing::WireTestBase<fuchsia_hardware_powerdomain::Domain> {
 public:
  fuchsia_hardware_powerdomain::Service::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_powerdomain::Service::InstanceHandler({
        .domain = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  uint32_t ref_count() const { return ref_count_; }
  uint32_t enable_calls() const { return enable_calls_; }
  uint32_t disable_calls() const { return disable_calls_; }
  bool is_enabled() const { return ref_count_ > 0; }

  void set_enable_result(zx::result<> result) { enable_result_ = result; }
  // Makes Disable fail with `result`, leaving `ref_count_` unchanged.
  void set_disable_result(zx::result<> result) { disable_result_ = result; }
  // Records successful Enable and Disable calls in `log`, as votes on `resource`.
  void set_log(Resource resource, EventLog* log) {
    resource_ = resource;
    log_ = log;
  }

 private:
  void Enable(EnableCompleter::Sync& completer) override {
    ++enable_calls_;
    if (enable_result_.is_error()) {
      completer.ReplyError(enable_result_.status_value());
      return;
    }
    if (log_) {
      log_->push_back(Enabled(resource_));
    }
    ++ref_count_;
    completer.ReplySuccess();
  }

  void Disable(DisableCompleter::Sync& completer) override {
    ++disable_calls_;
    if (disable_result_.is_error()) {
      completer.ReplyError(disable_result_.status_value());
      return;
    }
    if (log_) {
      log_->push_back(Disabled(resource_));
    }
    if (ref_count_ > 0) {
      --ref_count_;
    }
    completer.ReplySuccess();
  }

  void IsEnabled(IsEnabledCompleter::Sync& completer) override {
    completer.ReplySuccess(ref_count_ > 0);
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_powerdomain::Domain> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  uint32_t ref_count_ = 0;
  uint32_t enable_calls_ = 0;
  uint32_t disable_calls_ = 0;
  zx::result<> enable_result_ = zx::ok();
  zx::result<> disable_result_ = zx::ok();
  Resource resource_ = kPowerDomain;
  EventLog* log_ = nullptr;
  fidl::ServerBindingGroup<fuchsia_hardware_powerdomain::Domain> bindings_;
};

struct SharedRefCountedResources {
  SharedRefCountedResources() {
    powerdomain.set_log(kPowerDomain, &log);
    irq_controller.set_log(kIrqController, &log);
    clock_bus.set_log(kBusClock, &log);
    clock_regs.set_log(kRegistersClock, &log);
  }

  RefCountedPowerDomain powerdomain;
  RefCountedClock clock_bus;
  RefCountedClock clock_regs;
  // Only served as the "interrupt-controller-0" instance if `serve_irq_controller` is set.
  RefCountedPowerDomain irq_controller;
  bool serve_irq_controller = false;
  // Successful Enable and Disable calls on all of the above, in order.
  EventLog log;
};

class ControllableReset final : public fidl::testing::WireTestBase<fuchsia_hardware_reset::Reset> {
 public:
  fuchsia_hardware_reset::Service::InstanceHandler CreateInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_reset::Service::InstanceHandler({
        .reset = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  bool take_asserted() { return std::exchange(asserted_, false); }
  bool take_deasserted() { return std::exchange(deasserted_, false); }
  void set_assert_result(zx::result<> result) { assert_result_ = result; }
  void set_deassert_result(zx::result<> result) { deassert_result_ = result; }

 private:
  void Assert(AssertCompleter::Sync& completer) override {
    if (assert_result_.is_error()) {
      completer.ReplyError(assert_result_.status_value());
      return;
    }
    asserted_ = true;
    completer.ReplySuccess();
  }

  void Deassert(DeassertCompleter::Sync& completer) override {
    if (deassert_result_.is_error()) {
      completer.ReplyError(deassert_result_.status_value());
      return;
    }
    deasserted_ = true;
    completer.ReplySuccess();
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_reset::Reset> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  bool asserted_ = false;
  bool deasserted_ = false;
  zx::result<> assert_result_ = zx::ok();
  zx::result<> deassert_result_ = zx::ok();
  fidl::ServerBindingGroup<fuchsia_hardware_reset::Reset> bindings_;
};

class TestEnvironment : public fdf_testing::Environment {
 public:
  void Init(zx::interrupt interrupt, SharedRefCountedResources* shared_resources = nullptr) {
    std::map<uint32_t, zx::interrupt> irqs;
    irqs[0] = std::move(interrupt);
    pdev_.SetConfig({.irqs = std::move(irqs)});
    shared_resources_ = shared_resources;
  }

  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_.GetInstanceHandler(dispatcher), "pdev"));
    if (shared_resources_) {
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          shared_resources_->clock_bus.CreateInstanceHandler(dispatcher), "bus"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          shared_resources_->clock_regs.CreateInstanceHandler(dispatcher), "registers"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
          shared_resources_->powerdomain.CreateInstanceHandler(dispatcher), "power-domain"));
      if (shared_resources_->serve_irq_controller) {
        EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
            shared_resources_->irq_controller.CreateInstanceHandler(dispatcher),
            "interrupt-controller-0"));
      }
    } else {
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          clock_bus_.CreateInstanceHandler(dispatcher), "bus"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_clock::Service>(
          clock_regs_.CreateInstanceHandler(dispatcher), "registers"));
      EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_powerdomain::Service>(
          powerdomain_.CreateInstanceHandler(), "power-domain"));
    }
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_pin::PinStatesService>(
        pin_states_.CreateInstanceHandler(dispatcher), "pin-states-0"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_reset::Service>(
        reset_.CreateInstanceHandler(dispatcher), "reset"));
    return zx::ok();
  }

  fdf_fake::FakeClock& clock_bus() { return clock_bus_; }
  fdf_fake::FakeClock& clock_regs() { return clock_regs_; }
  FakePinStates& pin_states() { return pin_states_; }
  fdf_fake::FakePowerDomain& powerdomain() { return powerdomain_; }
  ControllableReset& reset() { return reset_; }

 private:
  fdf_fake::FakePDev pdev_;
  fdf_fake::FakeClock clock_bus_;
  fdf_fake::FakeClock clock_regs_;
  FakePinStates pin_states_;
  fdf_fake::FakePowerDomain powerdomain_;
  ControllableReset reset_;
  SharedRefCountedResources* shared_resources_ = nullptr;
};

class TestConfig final {
 public:
  using DriverType = TestDwI2c;
  using EnvironmentType = TestEnvironment;
};

using DriverTest = fdf_testing::BackgroundDriverTest<TestConfig>;

// Issues a two-byte write and returns the resulting status. Without a simulated wire the
// transaction times out after the driver's transfer timeout, but the controller is still powered
// up and initialized for it.
zx_status_t TransactWrite(fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device>& i2c) {
  fdf::Arena arena('TEST');
  std::array<uint8_t, 2> write_bytes = {0x01, 0x02};
  auto write_view = fidl::VectorView<uint8_t>::FromExternal(write_bytes.data(), write_bytes.size());
  std::array<fuchsia_hardware_i2cimpl::wire::I2cImplOp, 1> ops = {{
      {
          .address = 0x60,
          .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(
              fidl::ObjectView<fidl::VectorView<uint8_t>>::FromExternal(&write_view)),
          .stop = true,
      },
  }};
  auto result = i2c.buffer(arena)->Transact(
      fidl::VectorView<fuchsia_hardware_i2cimpl::wire::I2cImplOp>::FromExternal(ops.data(),
                                                                                ops.size()));
  if (!result.ok()) {
    return result.status();
  }
  return result->is_ok() ? ZX_OK : result->error_value();
}

// Connects to the driver's I2C impl service. Use with ASSERT_NO_FATAL_FAILURE().
void ConnectI2c(DriverTest& driver_test,
                fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device>* i2c) {
  zx::result client_end = driver_test.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
  ASSERT_OK(client_end);
  *i2c = fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device>(std::move(*client_end));
}

TEST(DwI2cTest, Lifecycle) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_TRUE(driver_test.StartDriver().is_ok());
  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
  });

  ASSERT_TRUE(driver_test.StopDriver().is_ok());
  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_asserted());
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
  });
}

// Regression test: when dw-i2c restarts (e.g. via `ffx driver restart`), Stop() must release its
// clock and power domain votes so the new driver instance starts with ref_count == 1 instead of 2.
// Otherwise SystemSuspend() only decrements ref_count from 2 to 1. If the platform power manager
// then forcibly collapses the power domain during ambient suspend (gating off the hardware clocks),
// the next power-up's Enable() sees ref_count == 1 != 0 and skips the hardware clock enable, and
// HostInit() reads 0x0 from CompTypeReg (0xfc). SystemResume() is lazy, so the test checks that the
// next transaction powers the controller up and reinitializes it.
TEST(DwI2cTest, RestartThenForcedAmbientSuspendAndResume) {
  SharedRefCountedResources shared_resources;
  FakeDwI2cController controller;
  controller.set_powered(false);
  shared_resources.clock_regs.set_on_hw_gate_change(
      [&](bool gated_on) { controller.set_powered(gated_on); });

  // First driver instance (before driver restart).
  {
    fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
    zx::interrupt interrupt;
    ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    driver_test.RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { env.Init(std::move(interrupt), &shared_resources); });
    TestDwI2c::set_mmio(controller.GetMmioBuffer());

    ASSERT_OK(driver_test.StartDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 1u);
      EXPECT_TRUE(shared_resources.clock_regs.hw_gated_on());
    });

    ASSERT_OK(driver_test.StopDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 0u);
      EXPECT_FALSE(shared_resources.clock_regs.hw_gated_on());
    });
  }

  // Second driver instance (after driver restart), followed by ambient suspend and resume.
  {
    fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
    zx::interrupt interrupt;
    ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    driver_test.RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { env.Init(std::move(interrupt), &shared_resources); });
    TestDwI2c::set_mmio(controller.GetMmioBuffer());

    ASSERT_OK(driver_test.StartDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 1u);
      EXPECT_TRUE(shared_resources.clock_regs.hw_gated_on());
    });

    zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
    driver_test.RunInDriverContext([&](TestDwI2c& driver) {
      driver.SystemSuspend(
          fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
    });
    ASSERT_OK(suspend_result);

    // Simulate ambient suspend collapsing the controller's power domain and gating off the hardware
    // clocks while register state is lost.
    controller.ResetWritableRegs();
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      shared_resources.clock_bus.ForceHardwareGateOff();
      shared_resources.clock_regs.ForceHardwareGateOff();
    });

    zx::result<> resume_result = zx::error(ZX_ERR_INTERNAL);
    driver_test.RunInDriverContext([&](TestDwI2c& driver) {
      driver.SystemResume(
          std::nullopt, fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
    });
    EXPECT_OK(resume_result);

    // Resume is lazy: nothing is powered until the next transaction.
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 0u);
    });

    driver_test.RunInDriverContext(
        [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });
    fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c;
    ASSERT_NO_FATAL_FAILURE(ConnectI2c(driver_test, &i2c));
    EXPECT_STATUS(TransactWrite(i2c), ZX_ERR_TIMED_OUT);

    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 1u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 1u);
      EXPECT_TRUE(shared_resources.clock_regs.hw_gated_on());
    });
    EXPECT_NE(controller.ReadReg(0x00), 0u);  // ControlReg

    ASSERT_OK(driver_test.StopDriver());
    driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
      EXPECT_EQ(shared_resources.powerdomain.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_bus.ref_count(), 0u);
      EXPECT_EQ(shared_resources.clock_regs.ref_count(), 0u);
    });
  }
}

TEST(DwI2cTest, StopWhileSuspendedDoesNotDoubleDisable) {
  SharedRefCountedResources shared_resources;
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;
  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
  driver_test.RunInEnvironmentTypeContext(
      [&](TestEnvironment& env) { env.Init(std::move(interrupt), &shared_resources); });

  ASSERT_OK(driver_test.StartDriver());

  zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);

  driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
    EXPECT_EQ(shared_resources.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_regs.disable_calls(), 1u);
  });

  ASSERT_OK(driver_test.StopDriver());
  driver_test.RunInEnvironmentTypeContext([&](TestEnvironment&) {
    EXPECT_EQ(shared_resources.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(shared_resources.clock_regs.disable_calls(), 1u);
  });
}

TEST(DwI2cTest, StartRollbackOnError) {
  // Simulate CompTypeReg returning 0x0 so HostInit() fails with ZX_ERR_NOT_SUPPORTED during Start.
  FakeDwI2cController controller;
  controller.set_powered(false);
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  EXPECT_STATUS(driver_test.StartDriver(), ZX_ERR_NOT_SUPPORTED);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_asserted());
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
  });
}

TEST(DwI2cTest, SuspendAndResume) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_deasserted());
  });
  EXPECT_NE(controller.ReadReg(0x00), 0u);  // ControlReg
  EXPECT_NE(controller.ReadReg(0x14), 0u);  // StandardSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x1c), 0u);  // FastSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x3c), 0u);  // TxFifoThresholdReg
  EXPECT_NE(controller.ReadReg(0x7c), 0u);  // SdaHoldReg
  EXPECT_NE(controller.ReadReg(0xac), 0u);  // SclStuckAtLowTimeoutReg

  zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_asserted());
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
  });

  // Simulate register and pin state lost while power domain is disabled.
  controller.ResetWritableRegs();
  controller.set_powered(false);
  driver_test.RunInEnvironmentTypeContext([&](TestEnvironment& env) {
    env.pin_states().set_on_select_state([&](std::string_view state) {
      if (state == "default") {
        controller.set_powered(true);
      }
    });
  });
  EXPECT_EQ(controller.ReadReg(0x00), 0u);
  EXPECT_EQ(controller.ReadReg(0x14), 0u);
  EXPECT_EQ(controller.ReadReg(0x1c), 0u);
  EXPECT_EQ(controller.ReadReg(0x3c), 0u);
  EXPECT_EQ(controller.ReadReg(0x7c), 0u);
  EXPECT_EQ(controller.ReadReg(0xac), 0u);

  zx::result<> resume_result = zx::error(ZX_ERR_INTERNAL);
  driver_test.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemResume(std::nullopt,
                        fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
  });
  ASSERT_OK(resume_result);

  // Resume is lazy: the controller stays off until the next transaction.
  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_FALSE(env.powerdomain().is_enabled());
    EXPECT_FALSE(env.clock_bus().enabled());
    EXPECT_FALSE(env.clock_regs().enabled());
    EXPECT_FALSE(env.reset().take_deasserted());
  });
  EXPECT_EQ(controller.ReadReg(0x00), 0u);

  driver_test.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c;
  ASSERT_NO_FATAL_FAILURE(ConnectI2c(driver_test, &i2c));
  EXPECT_STATUS(TransactWrite(i2c), ZX_ERR_TIMED_OUT);

  driver_test.RunInEnvironmentTypeContext([](TestEnvironment& env) {
    EXPECT_TRUE(env.powerdomain().is_enabled());
    EXPECT_TRUE(env.clock_bus().enabled());
    EXPECT_TRUE(env.clock_regs().enabled());
    EXPECT_TRUE(env.reset().take_deasserted());
    EXPECT_EQ(env.pin_states().last_state(), "default");
  });
  EXPECT_NE(controller.ReadReg(0x00), 0u);  // ControlReg
  EXPECT_NE(controller.ReadReg(0x14), 0u);  // StandardSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x1c), 0u);  // FastSpeedSclHighCountReg
  EXPECT_NE(controller.ReadReg(0x3c), 0u);  // TxFifoThresholdReg
  EXPECT_NE(controller.ReadReg(0x7c), 0u);  // SdaHoldReg
  EXPECT_NE(controller.ReadReg(0xac), 0u);  // SclStuckAtLowTimeoutReg

  ASSERT_OK(driver_test.StopDriver());
}

// Fixture for Transact tests. The fake controller models the bus and the level-triggered interrupt
// line, so no extra test threads are needed: each transfer completes inside the driver's
// InterruptStatusReg read just before it waits for the interrupt. The bus only advances on a zero
// status read while no interrupt is pending, so the driver must re-read the status after acking.
class DwI2cTransactTest : public ::testing::Test {
 protected:
  using I2cImplOp = fuchsia_hardware_i2cimpl::wire::I2cImplOp;
  static constexpr uint16_t kAddress = 0x60;

  void SetUp() override {
    TestDwI2c::set_mmio(controller_.GetMmioBuffer());

    zx::interrupt interrupt;
    ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    zx::interrupt controller_irq;
    ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &controller_irq));
    controller_.set_interrupt(std::move(controller_irq));
    driver_test_.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

    ASSERT_OK(driver_test_.StartDriver());
    driver_started_ = true;

    zx::result client_end = driver_test_.Connect<fuchsia_hardware_i2cimpl::Service::Device>();
    ASSERT_OK(client_end);
    i2c_ = fdf::WireSyncClient(std::move(*client_end));
  }

  void TearDown() override {
    if (driver_started_) {
      EXPECT_OK(driver_test_.StopDriver());
    }
  }

  I2cImplOp WriteOp(const std::vector<uint8_t>& data, bool stop) {
    fidl::VectorView<uint8_t> bytes(arena_, data);
    return {
        .address = kAddress,
        .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithWriteData(arena_, bytes),
        .stop = stop,
    };
  }

  static I2cImplOp ReadOp(uint32_t size, bool stop) {
    return {
        .address = kAddress,
        .type = fuchsia_hardware_i2cimpl::wire::I2cImplOpType::WithReadSize(size),
        .stop = stop,
    };
  }

  // Performs a transaction and returns the data received by its read ops.
  zx::result<std::vector<std::vector<uint8_t>>> Transact(std::vector<I2cImplOp> ops) {
    auto result = i2c_.buffer(arena_)->Transact(fidl::VectorView<I2cImplOp>::FromExternal(ops));
    EXPECT_OK(result.status());
    if (!result.ok()) {
      return zx::error(result.status());
    }
    if (result->is_error()) {
      return zx::error(result->error_value());
    }
    std::vector<std::vector<uint8_t>> reads;
    for (const auto& read : result->value()->read) {
      reads.emplace_back(read.data.begin(), read.data.end());
    }
    return zx::ok(std::move(reads));
  }

  // Checks that the driver masked interrupts, disabled the controller, and acknowledged the
  // interrupt only once it was no longer asserted.
  void ExpectIdle() {
    EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kIntrMask), 0u);
    EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kEnable), 0u);
    EXPECT_FALSE(controller_.IsInterruptTriggered());
    EXPECT_EQ(controller_.spurious_retrigger_count(), 0u);
  }

  // The controller must outlive the driver.
  FakeDwI2cController controller_;
  fdf_testing::BackgroundDriverTest<TestConfig> driver_test_;
  bool driver_started_ = false;
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c_;
  fdf::Arena arena_{'TEST'};
};

TEST_F(DwI2cTransactTest, WakesOnLevelIrqWithoutSpuriousRetrigger) {
  // Execute 3 back-to-back 2-byte writes followed by a 1-byte write + 2-byte read transaction.
  for (uint8_t i = 0; i < 3; ++i) {
    zx::result reads =
        Transact({WriteOp({static_cast<uint8_t>(0x10 + i), static_cast<uint8_t>(0x80 | i)},
                          /*stop=*/true)});
    ASSERT_OK(reads);
    EXPECT_TRUE(reads->empty());
    ExpectIdle();
  }

  controller_.QueueCannedRxBytes({0xa5, 0x5a});
  zx::result reads = Transact({WriteOp({0x42}, /*stop=*/false), ReadOp(2, /*stop=*/true)});
  ASSERT_OK(reads);
  ASSERT_EQ(reads->size(), 1u);
  EXPECT_EQ(reads->at(0), (std::vector<uint8_t>{0xa5, 0x5a}));
  ExpectIdle();

  // Each transaction is completed by a single STOP_DET interrupt.
  EXPECT_EQ(controller_.irq_trigger_count(), 4u);

  const std::vector<FakeDwI2cController::RecordedCommand> cmds = controller_.TakeTxCommands();
  ASSERT_EQ(cmds.size(), 9u);
  for (uint8_t i = 0; i < 3; ++i) {
    EXPECT_EQ(cmds[2 * i].data, 0x10 + i);
    EXPECT_FALSE(cmds[2 * i].stop);
    EXPECT_EQ(cmds[2 * i + 1].data, 0x80 | i);
    EXPECT_TRUE(cmds[2 * i + 1].stop);
  }
  EXPECT_EQ(cmds[6].target_addr, kAddress);
  EXPECT_FALSE(cmds[6].is_read);
  EXPECT_EQ(cmds[6].data, 0x42);
  EXPECT_FALSE(cmds[6].stop);
  EXPECT_TRUE(cmds[7].is_read);
  EXPECT_TRUE(cmds[7].start);
  EXPECT_FALSE(cmds[7].stop);
  EXPECT_TRUE(cmds[8].is_read);
  EXPECT_TRUE(cmds[8].stop);
}

using HealthCheckResult = fit::result<fuchsia_driver_framework::HealthCheckError,
                                      fuchsia_driver_framework::HealthCheckResponse>;

// Runs a synchronous HealthCheck on `driver` (must be called in driver context).
HealthCheckResult RunHealthCheck(TestDwI2c& driver,
                                 fuchsia_driver_framework::HealthCheckRequest level) {
  HealthCheckResult result = fit::error(fuchsia_driver_framework::HealthCheckError::kInternalError);
  driver.HealthCheck(level, [&](HealthCheckResult res) { result = std::move(res); });
  return result;
}

TEST_F(DwI2cTransactTest, TxAbortMasksBeforeAck) {
  // Use a transfer longer than the TX FIFO so TX_EMPTY is still unmasked when the abort is handled.
  // The abort flushes the TX FIFO, asserting TX_EMPTY, so acknowledging the interrupt before
  // masking it would re-trigger the level-triggered interrupt.
  controller_.AbortNextTransfer(0x1);

  EXPECT_STATUS(Transact({WriteOp({0, 1, 2, 3, 4, 5}, /*stop=*/false),
                          WriteOp({6, 7, 8, 9, 10, 11}, /*stop=*/true)}),
                ZX_ERR_IO);
  EXPECT_EQ(controller_.irq_trigger_count(), 1u);
  ExpectIdle();
  // Nothing is transmitted after the abort.
  EXPECT_EQ(controller_.TakeTxCommands().size(), 8u);

  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult ping_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(ping_res.is_ok());
    EXPECT_TRUE(!ping_res->conditions().has_value() || ping_res->conditions()->empty());
    ASSERT_TRUE(ping_res->diagnostics().has_value());

    bool found = false;
    for (const auto& item : *ping_res->diagnostics()) {
      if (item.key() == DwI2c::kDiagTxAbortCount) {
        EXPECT_EQ(item.value().uint_value().value(), 1u);
        found = true;
      }
    }
    EXPECT_TRUE(found);
  });
}

TEST_F(DwI2cTransactTest, TimeoutMasksAndDisables) {
  controller_.StallBus();
  driver_test_.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });

  const zx::time start = zx::clock::get_monotonic();
  zx::result reads = Transact({WriteOp({0x01, 0x02}, /*stop=*/true)});
  const zx::duration elapsed = zx::clock::get_monotonic() - start;

  EXPECT_STATUS(reads, ZX_ERR_TIMED_OUT);
  EXPECT_GE(elapsed, zx::msec(5));
  EXPECT_EQ(controller_.irq_trigger_count(), 0u);
  ExpectIdle();
}

TEST_F(DwI2cTransactTest, MultiChunkTxEmptyAndStopDetLevelIrq) {
  // Two 6-byte WriteOps = 12 commands (> tx_fifo_depth_ == 8), while each op is <= kMaxTransfer
  // (8), forcing a priming 8-command chunk + TX_EMPTY wakeup + 4-command chunk + STOP_DET wakeup.
  const std::vector<uint8_t> chunk0 = {0x10, 0x11, 0x12, 0x13, 0x14, 0x15};
  const std::vector<uint8_t> chunk1 = {0x20, 0x21, 0x22, 0x23, 0x24, 0x25};
  ASSERT_OK(Transact({WriteOp(chunk0, /*stop=*/false), WriteOp(chunk1, /*stop=*/true)}));
  EXPECT_EQ(controller_.irq_trigger_count(), 2u);
  ExpectIdle();

  const std::vector<FakeDwI2cController::RecordedCommand> cmds = controller_.TakeTxCommands();
  ASSERT_EQ(cmds.size(), 12u);
  for (size_t i = 0; i < 6; ++i) {
    EXPECT_EQ(cmds[i].data, chunk0[i]);
    EXPECT_FALSE(cmds[i].is_read);
    EXPECT_FALSE(cmds[i].stop);
  }
  for (size_t i = 0; i < 6; ++i) {
    EXPECT_EQ(cmds[6 + i].data, chunk1[i]);
    EXPECT_FALSE(cmds[6 + i].is_read);
    EXPECT_EQ(cmds[6 + i].start, i == 0);
    EXPECT_EQ(cmds[6 + i].stop, i == 5);
  }
}

TEST_F(DwI2cTransactTest, DrainsRetriggeredInterruptOnAck) {
  // Trigger the interrupt again when ClearStopDetReg is read (before the end-of-transaction ack)
  // so acknowledging the first interrupt re-queues a packet on irq_port_.
  controller_.RetriggerOnNextStopDetClear();
  ASSERT_OK(Transact({WriteOp({0x33, 0x44}, /*stop=*/true)}));
  EXPECT_EQ(controller_.irq_trigger_count(), 2u);

  // Verify the re-queued interrupt packet was drained and acknowledged before Transact returned.
  ExpectIdle();

  // Verify a subsequent transaction still wakes cleanly via irq_port_.
  ASSERT_OK(Transact({WriteOp({0x33, 0x44}, /*stop=*/true)}));
  EXPECT_EQ(controller_.irq_trigger_count(), 3u);
  ExpectIdle();
}

TEST(DwI2cTest, HealthCheckHealthyPingFastAndDeep) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());

  driver_test.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult ping_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(ping_res.is_ok());
    EXPECT_TRUE(!ping_res->conditions().has_value() || ping_res->conditions()->empty());
    ASSERT_TRUE(ping_res->diagnostics().has_value());
    EXPECT_FALSE(ping_res->diagnostics()->empty());

    HealthCheckResult fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(fast_res.is_ok());
    EXPECT_TRUE(!fast_res->conditions().has_value() || fast_res->conditions()->empty());
    ASSERT_TRUE(fast_res->diagnostics().has_value());
    EXPECT_FALSE(fast_res->diagnostics()->empty());

    HealthCheckResult deep_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kDeep);
    ASSERT_TRUE(deep_res.is_error());
    EXPECT_EQ(deep_res.error_value(), fuchsia_driver_framework::HealthCheckError::kNotSupported);
  });

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, FastRaisesOnceAndClearsUnclockedMmioRead) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());

  // Simulate unclocked MMIO read (CompTypeReg returning 0x0 while the driver considers the
  // controller powered).
  controller.set_powered(false);
  driver_test.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(fast_res.is_ok());
    ASSERT_TRUE(fast_res->conditions().has_value());
    ASSERT_EQ(fast_res->conditions()->size(), 1u);
    EXPECT_EQ(fast_res->conditions()->at(0).reason_code(), DwI2c::kReasonUnclockedMmioRead);
    EXPECT_EQ(fast_res->conditions()->at(0).status(),
              fuchsia_driver_framework::HealthStatus::kUnhealthy);
    EXPECT_EQ(fast_res->conditions()->at(0).occurrence_count(), 1u);

    // Status registers must not be read when CompTypeReg indicates an unclocked APB window.
    ASSERT_TRUE(fast_res->diagnostics().has_value());
    for (const auto& item : *fast_res->diagnostics()) {
      EXPECT_NE(item.key(), DwI2c::kDiagEnableStatus);
      EXPECT_NE(item.key(), DwI2c::kDiagStatus);
      EXPECT_NE(item.key(), DwI2c::kDiagRawIntrStat);
      EXPECT_NE(item.key(), DwI2c::kDiagTxAbrtSource);
    }

    // Repeated kFast polls in the same unclocked episode increment unclocked_read_count without
    // re-raising the latched condition or inflating occurrence_count.
    HealthCheckResult second_fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(second_fast_res.is_ok());
    ASSERT_TRUE(second_fast_res->conditions().has_value());
    ASSERT_EQ(second_fast_res->conditions()->size(), 1u);
    EXPECT_EQ(second_fast_res->conditions()->at(0).occurrence_count(), 1u);
    bool found_unclocked_count = false;
    for (const auto& item : *second_fast_res->diagnostics()) {
      if (item.key() == DwI2c::kDiagUnclockedReadCount) {
        EXPECT_EQ(item.value().uint_value().value(), 2u);
        found_unclocked_count = true;
      }
    }
    EXPECT_TRUE(found_unclocked_count);

    // Ping must also report the active I2C_UNCLOCKED_MMIO_READ condition.
    HealthCheckResult ping_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(ping_res.is_ok());
    ASSERT_TRUE(ping_res->conditions().has_value());
    ASSERT_EQ(ping_res->conditions()->size(), 1u);
    EXPECT_EQ(ping_res->conditions()->at(0).reason_code(), DwI2c::kReasonUnclockedMmioRead);
    EXPECT_EQ(ping_res->conditions()->at(0).status(),
              fuchsia_driver_framework::HealthStatus::kUnhealthy);
  });

  // Restore power and verify Fast clears I2C_UNCLOCKED_MMIO_READ.
  controller.set_powered(true);
  driver_test.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(fast_res.is_ok());
    EXPECT_TRUE(!fast_res->conditions().has_value() || fast_res->conditions()->empty());
  });

  ASSERT_OK(driver_test.StopDriver());
}

TEST(DwI2cTest, FastSclStuckLowStaysLatchedUntilTransfer) {
  FakeDwI2cController controller;
  TestDwI2c::set_mmio(controller.GetMmioBuffer());

  fdf_testing::BackgroundDriverTest<TestConfig> driver_test;

  zx::interrupt interrupt;
  ASSERT_EQ(ZX_OK, zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));

  driver_test.RunInEnvironmentTypeContext([&](auto& env) { env.Init(std::move(interrupt)); });

  ASSERT_OK(driver_test.StartDriver());

  controller.set_scl_stuck_low(true);
  driver_test.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(fast_res.is_ok());
    ASSERT_TRUE(fast_res->conditions().has_value());
    ASSERT_EQ(fast_res->conditions()->size(), 1u);
    EXPECT_EQ(fast_res->conditions()->at(0).reason_code(), DwI2c::kReasonSclStuckLow);
    EXPECT_EQ(fast_res->conditions()->at(0).occurrence_count(), 1u);

    // A second kFast poll while the bit remains set does not inflate occurrence_count.
    HealthCheckResult second_fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(second_fast_res.is_ok());
    ASSERT_TRUE(second_fast_res->conditions().has_value());
    ASSERT_EQ(second_fast_res->conditions()->size(), 1u);
    EXPECT_EQ(second_fast_res->conditions()->at(0).occurrence_count(), 1u);
  });

  // The sticky status bit reading clear again is not evidence of recovery (the driver wipes it
  // after every transfer attempt), so Fast keeps I2C_SCL_STUCK_LOW latched without re-raising it.
  // Only a completed transfer clears it (see SuccessfulTransactClearsLatchedFaults).
  controller.set_scl_stuck_low(false);
  driver_test.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(fast_res.is_ok());
    ASSERT_TRUE(fast_res->conditions().has_value());
    ASSERT_EQ(fast_res->conditions()->size(), 1u);
    EXPECT_EQ(fast_res->conditions()->at(0).reason_code(), DwI2c::kReasonSclStuckLow);
    EXPECT_EQ(fast_res->conditions()->at(0).occurrence_count(), 1u);
  });

  ASSERT_OK(driver_test.StopDriver());
}

// The controller is not powered up just for a health check: while it is suspended, kFast reports
// what kPing reports (the latched faults and counters) without touching the registers. Resume is
// lazy, so a failure to power the controller up surfaces on the next transaction, and is latched
// as I2C_RESUME_FAILED until the controller is powered up again.
TEST_F(DwI2cTransactTest, HealthCheckWhileSuspendedAndAfterPowerUpFailure) {
  zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
  driver_test_.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);

  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (fuchsia_driver_framework::HealthCheckRequest level :
         {fuchsia_driver_framework::HealthCheckRequest::kPing,
          fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      EXPECT_TRUE(!res->conditions().has_value() || res->conditions()->empty());
      ASSERT_TRUE(res->diagnostics().has_value());
      bool found_resources_enabled = false;
      for (const auto& item : *res->diagnostics()) {
        // No register was sampled.
        EXPECT_NE(item.key(), DwI2c::kDiagCompType);
        if (item.key() == DwI2c::kDiagResourcesEnabled) {
          EXPECT_FALSE(item.value().bool_value().value());
          found_resources_enabled = true;
        }
      }
      EXPECT_TRUE(found_resources_enabled);
    }
  });

  zx::result<> resume_result = zx::error(ZX_ERR_INTERNAL);
  driver_test_.RunInDriverContext([&](TestDwI2c& driver) {
    driver.SystemResume(std::nullopt,
                        fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
  });
  ASSERT_OK(resume_result);

  // The next transaction fails to power the controller up. Both Ping and Fast report
  // I2C_RESUME_FAILED (kUnhealthy) while the controller stays unpowered.
  driver_test_.RunInEnvironmentTypeContext(
      [](TestEnvironment& env) { env.clock_regs().set_enable_result(zx::error(ZX_ERR_IO)); });
  EXPECT_STATUS(Transact({WriteOp({0x01}, /*stop=*/true)}), ZX_ERR_IO);
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy, driver.health_latch().status());
    for (fuchsia_driver_framework::HealthCheckRequest level :
         {fuchsia_driver_framework::HealthCheckRequest::kPing,
          fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      ASSERT_TRUE(res->conditions().has_value());
      ASSERT_EQ(1u, res->conditions()->size());
      EXPECT_EQ(DwI2c::kReasonResumeFailed, res->conditions()->at(0).reason_code());
      EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kUnhealthy,
                res->conditions()->at(0).status());
    }
  });

  // A second failure doesn't add a new condition but increments occurrence_count.
  EXPECT_STATUS(Transact({WriteOp({0x01}, /*stop=*/true)}), ZX_ERR_IO);
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    std::vector<fuchsia_driver_framework::HealthCondition> snap = driver.health_latch().Snapshot();
    ASSERT_EQ(1u, snap.size());
    EXPECT_EQ(2u, snap[0].occurrence_count());
  });

  // Once the controller powers up and a transfer completes, the latch is clear.
  driver_test_.RunInEnvironmentTypeContext(
      [](TestEnvironment& env) { env.clock_regs().set_enable_result(zx::ok()); });
  ASSERT_OK(Transact({WriteOp({0x02}, /*stop=*/true)}));
  ExpectIdle();
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, driver.health_latch().status());
    EXPECT_TRUE(driver.health_latch().Snapshot().empty());
  });
}

// A completed transfer is the recovery proof for every fault the driver raises: with two faults
// latched (a transaction timeout and SCL stuck low observed by Fast), one successful Transact
// clears the whole set at once.
TEST_F(DwI2cTransactTest, SuccessfulTransactClearsLatchedFaults) {
  controller_.StallBus(/*scl_stuck_low=*/true);
  driver_test_.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });
  EXPECT_STATUS(Transact({WriteOp({0x01}, /*stop=*/true)}), ZX_ERR_TIMED_OUT);
  controller_.UnstallBus();

  // Post-transfer cleanup already wiped SCL_STUCK_AT_LOW in RawInterruptStatusReg, yet both
  // conditions were latched in WaitForInterrupt() and stay latched across HealthCheck(kFast).
  EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kRawIntrStat), 0u);
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult fast_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kFast);
    ASSERT_TRUE(fast_res.is_ok());
    ASSERT_TRUE(fast_res->conditions().has_value());
    EXPECT_EQ(fast_res->conditions()->size(), 2u);

    std::vector<std::string> codes;
    for (const auto& cond : driver.health_latch().Snapshot()) {
      codes.push_back(cond.reason_code().value_or(""));
    }
    std::sort(codes.begin(), codes.end());
    EXPECT_EQ(codes, (std::vector<std::string>{std::string(DwI2c::kReasonSclStuckLow),
                                               std::string(DwI2c::kReasonTransactionTimeout)}));
  });

  ASSERT_OK(Transact({WriteOp({0x02}, /*stop=*/true)}));
  ExpectIdle();
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    EXPECT_TRUE(driver.health_latch().Snapshot().empty());
    EXPECT_EQ(fuchsia_driver_framework::HealthStatus::kOk, driver.health_latch().status());
  });
}

// A controller that completes the transfer but then refuses to disable still returns the
// transfer's data, latches I2C_DISABLE_TIMEOUT (which the transfer did not disprove), and clears
// it on the next transfer that disables cleanly.
TEST_F(DwI2cTransactTest, DisableTimeoutIsLatchedAndClearedByCleanTransfer) {
  controller_.StickEnabled(true);
  ASSERT_OK(Transact({WriteOp({0x11}, /*stop=*/true)}));
  EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kEnable), 0u);
  EXPECT_EQ(controller_.ReadReg(FakeDwI2cController::kEnableStatus), 1u);

  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    std::vector<fuchsia_driver_framework::HealthCondition> snap = driver.health_latch().Snapshot();
    ASSERT_EQ(snap.size(), 1u);
    EXPECT_EQ(snap[0].reason_code(), DwI2c::kReasonDisableTimeout);
    EXPECT_EQ(snap[0].status(), fuchsia_driver_framework::HealthStatus::kDegraded);

    HealthCheckResult ping_res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(ping_res.is_ok());
    ASSERT_TRUE(ping_res->conditions().has_value());
    ASSERT_EQ(ping_res->conditions()->size(), 1u);
    EXPECT_EQ(ping_res->conditions()->at(0).reason_code(), DwI2c::kReasonDisableTimeout);
    bool found = false;
    for (const auto& item : *ping_res->diagnostics()) {
      if (item.key() == DwI2c::kDiagTimeoutCount) {
        EXPECT_EQ(item.value().uint_value().value(), 1u);
        found = true;
      }
    }
    EXPECT_TRUE(found);
  });

  controller_.StickEnabled(false);
  ASSERT_OK(Transact({WriteOp({0x12}, /*stop=*/true)}));
  ExpectIdle();
  driver_test_.RunInDriverContext(
      [](TestDwI2c& driver) { EXPECT_TRUE(driver.health_latch().Snapshot().empty()); });
}

// Fixture for runtime PM tests. The power domain, clocks and interrupt controller are reference
// counted like the platform providers, and the controller registers are only accessible while the
// registers clock (pclk) is on: any access while it is off fails the test.
class DwI2cRuntimePmTest : public ::testing::Test {
 protected:
  void SetUp() override {
    controller_.set_powered(false);
    controller_.set_fail_on_unpowered_access(true);
    resources_.clock_regs.set_on_hw_gate_change(
        [this](bool gated_on) { controller_.set_powered(gated_on); });
    TestDwI2c::set_mmio(controller_.GetMmioBuffer());

    zx::interrupt interrupt;
    ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt));
    ASSERT_OK(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &controller_irq_));
    driver_test_.RunInEnvironmentTypeContext(
        [&](TestEnvironment& env) { env.Init(std::move(interrupt), &resources_); });
  }

  void TearDown() override {
    if (driver_started_) {
      EXPECT_OK(driver_test_.StopDriver());
    }
  }

  // Enables simulated bus completions and level-triggered interrupts on `controller_`, so
  // `TransactWrite()` completes with ZX_OK instead of timing out. Must be called before
  // `StartDriver()`.
  void EnableSimulatedBus() { controller_.set_interrupt(std::move(controller_irq_)); }

  // Starts the driver and connects to it. Transactions use a short transfer timeout: without a
  // simulated bus they time out, but the controller is still powered up and initialized for them.
  void StartDriver() {
    ASSERT_OK(driver_test_.StartDriver());
    driver_started_ = true;
    driver_test_.RunInDriverContext(
        [](TestDwI2c& driver) { driver.set_transfer_timeout(zx::msec(5)); });
    ASSERT_NO_FATAL_FAILURE(ConnectI2c(driver_test_, &i2c_));
  }

  void StopDriver() {
    driver_started_ = false;
    ASSERT_OK(driver_test_.StopDriver());
  }

  // Runs `callback` on the environment dispatcher, which serves the fakes. `callback` takes either
  // no argument or the `TestEnvironment`.
  template <typename Callback>
  void RunInEnvironment(Callback callback) {
    driver_test_.RunInEnvironmentTypeContext([&](TestEnvironment& env) {
      if constexpr (std::is_invocable_v<Callback, TestEnvironment&>) {
        callback(env);
      } else {
        callback();
      }
    });
  }

  // Expects `count` votes on the power domain and on both clocks.
  void ExpectVotes(uint32_t count) {
    RunInEnvironment([&] {
      EXPECT_EQ(resources_.powerdomain.ref_count(), count);
      EXPECT_EQ(resources_.clock_bus.ref_count(), count);
      EXPECT_EQ(resources_.clock_regs.ref_count(), count);
    });
  }

  // Returns and clears the calls recorded since the last call.
  EventLog TakeLog() {
    EventLog log;
    RunInEnvironment([&] { log = std::exchange(resources_.log, {}); });
    return log;
  }

  zx_status_t TransactWrite() { return dw_i2c::TransactWrite(i2c_); }

  bool IsPowered() {
    bool powered = false;
    driver_test_.RunInDriverContext([&](TestDwI2c& driver) { powered = driver.powered(); });
    return powered;
  }

  zx::result<> SystemSuspend() {
    zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
    driver_test_.RunInDriverContext([&](TestDwI2c& driver) {
      driver.SystemSuspend(
          fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
    });
    return suspend_result;
  }

  zx::result<> SystemResume() {
    zx::result<> resume_result = zx::error(ZX_ERR_INTERNAL);
    driver_test_.RunInDriverContext([&](TestDwI2c& driver) {
      driver.SystemResume(
          std::nullopt, fdf::ResumeCompleter([&](zx::result<> result) { resume_result = result; }));
    });
    return resume_result;
  }

  // Enables autosuspend with a short delay and waits for the driver to runtime suspend, then
  // disables autosuspend again so that the rest of the test is not timing sensitive.
  void ForceRuntimeSuspend() {
    driver_test_.RunInDriverContext(
        [](TestDwI2c& driver) { driver.set_autosuspend_delay(zx::msec(1)); });
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (IsPowered()) {
      if (zx::clock::get_monotonic() > deadline) {
        ADD_FAILURE() << "controller did not runtime suspend";
        break;
      }
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    driver_test_.RunInDriverContext(
        [](TestDwI2c& driver) { driver.set_autosuspend_delay(zx::duration::infinite()); });
  }

  static const fuchsia_driver_framework::HealthCondition* FindCondition(
      const fuchsia_driver_framework::HealthCheckResponse& response, std::string_view reason_code) {
    if (!response.conditions().has_value()) {
      return nullptr;
    }
    for (const auto& cond : *response.conditions()) {
      if (cond.reason_code() == reason_code) {
        return &cond;
      }
    }
    return nullptr;
  }

  static std::optional<uint64_t> FindDiagUint(
      const std::optional<std::vector<fuchsia_driver_framework::HealthDiagnosticItem>>& diagnostics,
      std::string_view key) {
    if (!diagnostics.has_value()) {
      return std::nullopt;
    }
    for (const auto& item : *diagnostics) {
      if (item.key() == key) {
        return item.value().uint_value();
      }
    }
    return std::nullopt;
  }

  static std::optional<std::string> FindDiagString(
      const std::optional<std::vector<fuchsia_driver_framework::HealthDiagnosticItem>>& diagnostics,
      std::string_view key) {
    if (!diagnostics.has_value()) {
      return std::nullopt;
    }
    for (const auto& item : *diagnostics) {
      if (item.key() == key) {
        return item.value().string_value();
      }
    }
    return std::nullopt;
  }

  // Reference-counted resources and the controller must outlive the driver.
  SharedRefCountedResources resources_;
  FakeDwI2cController controller_;
  zx::interrupt controller_irq_;
  DriverTest driver_test_;
  bool driver_started_ = false;
  fdf::WireSyncClient<fuchsia_hardware_i2cimpl::Device> i2c_;
};

TEST(DwI2cTest, DefaultAutosuspendDelay) { EXPECT_EQ(TestDwI2c::kAutosuspendDelay, zx::msec(200)); }

TEST_F(DwI2cRuntimePmTest, RuntimeSuspendAfterIdleFollowingStart) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_TRUE(IsPowered());
  ExpectVotes(1);

  ForceRuntimeSuspend();

  // No votes are held once the controller has been idle for the autosuspend delay, and the
  // controller was quiesced before power was removed and is held in reset.
  ExpectVotes(0);
  RunInEnvironment([](TestEnvironment& env) {
    EXPECT_TRUE(env.reset().take_asserted());
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
  });
  EXPECT_EQ(controller_.ReadReg(0x30), 0u);  // InterruptMaskReg
  EXPECT_EQ(controller_.ReadReg(0x6c), 0u);  // EnableReg

  // Stopping while runtime suspended must not release any vote again.
  ASSERT_NO_FATAL_FAILURE(StopDriver());
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.enable_calls(), 1u);
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_bus.enable_calls(), 1u);
    EXPECT_EQ(resources_.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.enable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 1u);
  });
}

TEST_F(DwI2cRuntimePmTest, TransactPowersUpAndReinitializesAfterRuntimeSuspend) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ForceRuntimeSuspend();

  // Simulate register state being lost while the power domain was off.
  controller_.ResetWritableRegs();
  EXPECT_EQ(controller_.ReadReg(0x00), 0u);

  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);

  EXPECT_TRUE(IsPowered());
  ExpectVotes(1);
  RunInEnvironment([&](TestEnvironment& env) {
    EXPECT_EQ(resources_.powerdomain.enable_calls(), 2u);
    EXPECT_TRUE(env.reset().take_deasserted());
    EXPECT_EQ(env.pin_states().last_state(), "default");
  });
  // HostInit() ran again.
  EXPECT_NE(controller_.ReadReg(0x00), 0u);  // ControlReg
  EXPECT_NE(controller_.ReadReg(0x14), 0u);  // StandardSpeedSclHighCountReg
  EXPECT_NE(controller_.ReadReg(0x1c), 0u);  // FastSpeedSclHighCountReg
  EXPECT_NE(controller_.ReadReg(0x3c), 0u);  // TxFifoThresholdReg
  EXPECT_NE(controller_.ReadReg(0x7c), 0u);  // SdaHoldReg

  // The controller is runtime suspended again once the bus is idle.
  ForceRuntimeSuspend();
  ExpectVotes(0);
}

TEST_F(DwI2cRuntimePmTest, BackToBackTransactionsDoNotPowerCycle) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  // Long enough that the timer never fires between the transactions below.
  driver_test_.RunInDriverContext(
      [](TestDwI2c& driver) { driver.set_autosuspend_delay(zx::sec(60)); });

  for (int i = 0; i < 3; ++i) {
    EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  }

  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.enable_calls(), 1u);
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 0u);
    EXPECT_EQ(resources_.clock_bus.enable_calls(), 1u);
    EXPECT_EQ(resources_.clock_bus.disable_calls(), 0u);
    EXPECT_EQ(resources_.clock_regs.enable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 0u);
  });

  ASSERT_NO_FATAL_FAILURE(StopDriver());
  ExpectVotes(0);
}

TEST_F(DwI2cRuntimePmTest, SystemSuspendWhileRuntimeSuspended) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ForceRuntimeSuspend();

  ASSERT_OK(SystemSuspend());
  ASSERT_OK(SystemResume());

  // Neither system suspend nor resume touched any vote.
  EXPECT_FALSE(IsPowered());
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.enable_calls(), 1u);
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_bus.enable_calls(), 1u);
    EXPECT_EQ(resources_.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.enable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 1u);
  });
}

TEST_F(DwI2cRuntimePmTest, SystemSuspendWhileRuntimePoweredCancelsAutosuspend) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  zx::result<> suspend_result = zx::error(ZX_ERR_INTERNAL);
  driver_test_.RunInDriverContext([&](TestDwI2c& driver) {
    // Arm the autosuspend timer, then system suspend before it fires.
    driver.set_autosuspend_delay(zx::msec(20));
    driver.SystemSuspend(
        fdf::SuspendCompleter([&](zx::result<> result) { suspend_result = result; }));
  });
  ASSERT_OK(suspend_result);
  EXPECT_FALSE(IsPowered());

  // The cancelled autosuspend timer must not release the votes a second time.
  zx::nanosleep(zx::deadline_after(zx::msec(50)));
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_bus.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 1u);
  });
  ExpectVotes(0);

  ASSERT_NO_FATAL_FAILURE(StopDriver());
  RunInEnvironment([&] { EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u); });
}

// A failed vote release during power-down treats the controller as unpowered without touching MMIO,
// latches I2C_POWER_RELEASE_FAILED in HealthCheck, and is never retried on subsequent power-ups,
// suspends, or stop. A completed transfer clears transfer faults while keeping
// I2C_POWER_RELEASE_FAILED latched permanently (since the leaked vote remains held until reboot).
TEST_F(DwI2cRuntimePmTest, PowerDownFailureLatchesPowerReleaseFailedWithoutRetrying) {
  resources_.serve_irq_controller = true;
  EnableSimulatedBus();
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  RunInEnvironment([&] { resources_.powerdomain.set_disable_result(zx::error(ZX_ERR_TIMED_OUT)); });
  ForceRuntimeSuspend();

  EXPECT_FALSE(IsPowered());
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 1u);
    EXPECT_EQ(resources_.clock_bus.ref_count(), 0u);
    EXPECT_EQ(resources_.clock_regs.ref_count(), 0u);
    EXPECT_EQ(resources_.irq_controller.ref_count(), 0u);
    resources_.powerdomain.set_disable_result(zx::ok());
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      ASSERT_TRUE(res->conditions().has_value());
      EXPECT_EQ(res->conditions()->size(), 1u);
      const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
      ASSERT_NE(cond, nullptr);
      EXPECT_EQ(cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(cond->occurrence_count(), 1u);
      EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "disable power domain");
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 1u);
      EXPECT_FALSE(FindDiagUint(res->diagnostics(), DwI2c::kDiagCompType).has_value());
    }
  });
  TakeLog();

  // Powering back up takes fresh votes without retrying the failed Disable; latch transfer faults
  // as well so we can verify what a succeeding transfer clears.
  controller_.StallBus(/*scl_stuck_low=*/true);
  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  controller_.UnstallBus();
  EXPECT_EQ(TakeLog(), (EventLog{Enabled(kPowerDomain), Enabled(kIrqController),
                                 Enabled(kRegistersClock), Enabled(kBusClock)}));
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 2u);
  });
  driver_test_.RunInDriverContext(
      [](TestDwI2c& driver) { EXPECT_EQ(driver.health_latch().Snapshot().size(), 3u); });

  // A completed transfer clears transfer faults (timeout + SCL stuck low), but keeps
  // I2C_POWER_RELEASE_FAILED latched because the leaked vote is never retried.
  EXPECT_OK(TransactWrite());
  RunInEnvironment([&] { EXPECT_EQ(resources_.powerdomain.disable_calls(), 1u); });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      ASSERT_TRUE(res->conditions().has_value());
      EXPECT_EQ(res->conditions()->size(), 1u);
      const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
      ASSERT_NE(cond, nullptr);
      EXPECT_EQ(cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(cond->occurrence_count(), 1u);
    }
  });

  // A subsequent clean power-down releases only the new vote (leaving ref_count == 1) and keeps
  // I2C_POWER_RELEASE_FAILED latched.
  ForceRuntimeSuspend();
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 2u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 1u);
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    EXPECT_EQ(driver.health_latch().status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
      ASSERT_NE(cond, nullptr);
      EXPECT_EQ(cond->occurrence_count(), 1u);
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 1u);
    }
  });

  // Failing to release the interrupt controller power domain vote also updates
  // I2C_POWER_RELEASE_FAILED while still disabling the controller's own power domain since the
  // clocks disabled cleanly.
  EXPECT_OK(TransactWrite());
  TakeLog();
  RunInEnvironment([&] { resources_.irq_controller.set_disable_result(zx::error(ZX_ERR_IO)); });
  ForceRuntimeSuspend();
  EXPECT_EQ(TakeLog(),
            (EventLog{Disabled(kBusClock), Disabled(kRegistersClock), Disabled(kPowerDomain)}));
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
    ASSERT_NE(cond, nullptr);
    EXPECT_EQ(cond->occurrence_count(), 2u);
    EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason),
              "disable interrupt controller power domain");
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 2u);
  });
  RunInEnvironment([&] { resources_.irq_controller.set_disable_result(zx::ok()); });

  // Neither SystemSuspend nor Stop retries the leaked votes while unpowered.
  EXPECT_OK(SystemSuspend());
  ASSERT_NO_FATAL_FAILURE(StopDriver());
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 3u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 1u);
    EXPECT_EQ(resources_.irq_controller.ref_count(), 1u);
  });
}

TEST_F(DwI2cRuntimePmTest, InterruptControllerVoteOrdering) {
  resources_.serve_irq_controller = true;
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  EXPECT_EQ(TakeLog(), (EventLog{Enabled(kPowerDomain), Enabled(kIrqController),
                                 Enabled(kRegistersClock), Enabled(kBusClock)}));
  RunInEnvironment([&] { EXPECT_EQ(resources_.irq_controller.ref_count(), 1u); });

  ForceRuntimeSuspend();
  EXPECT_EQ(TakeLog(), (EventLog{Disabled(kBusClock), Disabled(kRegistersClock),
                                 Disabled(kIrqController), Disabled(kPowerDomain)}));
  RunInEnvironment([&] { EXPECT_EQ(resources_.irq_controller.ref_count(), 0u); });

  // A transaction takes the interrupt controller vote again, in the same order.
  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  EXPECT_EQ(TakeLog(), (EventLog{Enabled(kPowerDomain), Enabled(kIrqController),
                                 Enabled(kRegistersClock), Enabled(kBusClock)}));

  ASSERT_NO_FATAL_FAILURE(StopDriver());
  EXPECT_EQ(TakeLog(), (EventLog{Disabled(kBusClock), Disabled(kRegistersClock),
                                 Disabled(kIrqController), Disabled(kPowerDomain)}));
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.irq_controller.ref_count(), 0u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 0u);
  });
}

TEST_F(DwI2cRuntimePmTest, PowerUpFailureRollsBackInterruptControllerVote) {
  resources_.serve_irq_controller = true;
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ForceRuntimeSuspend();

  RunInEnvironment([&] { resources_.clock_regs.set_enable_result(zx::error(ZX_ERR_IO)); });
  TakeLog();
  EXPECT_STATUS(TransactWrite(), ZX_ERR_IO);
  EXPECT_FALSE(IsPowered());
  EXPECT_EQ(TakeLog(), (EventLog{Enabled(kPowerDomain), Enabled(kIrqController),
                                 Disabled(kIrqController), Disabled(kPowerDomain)}));
  ExpectVotes(0);
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.irq_controller.ref_count(), 0u);
    resources_.clock_regs.set_enable_result(zx::ok());
  });

  // A failed interrupt controller vote also fails the power-up and rolls back the power domain.
  RunInEnvironment(
      [&] { resources_.irq_controller.set_enable_result(zx::error(ZX_ERR_BAD_STATE)); });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_BAD_STATE);
  EXPECT_FALSE(IsPowered());
  EXPECT_EQ(TakeLog(), (EventLog{Enabled(kPowerDomain), Disabled(kPowerDomain)}));
  RunInEnvironment([&] { resources_.irq_controller.set_enable_result(zx::ok()); });

  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  EXPECT_TRUE(IsPowered());

  ASSERT_NO_FATAL_FAILURE(StopDriver());
  ExpectVotes(0);
  RunInEnvironment([&] { EXPECT_EQ(resources_.irq_controller.ref_count(), 0u); });
}

// When disabling a clock fails during runtime or system suspend, PowerDomain::Disable is skipped so
// the power domain vote stays held while a clock is still enabled, MMIO is never touched until a
// subsequent power-up succeeds, SystemSuspend still returns ZX_OK, and HealthCheck surfaces
// I2C_POWER_RELEASE_FAILED with the latest failed clock action.
TEST_F(DwI2cRuntimePmTest, ClockDisableFailureSkipsPowerDomainDisableAndLatchesPowerReleaseFailed) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  TakeLog();

  // Case 1: registers clock Disable fails on runtime suspend. Force `controller_` unpowered even
  // though `clock_regs` kept its ref_count > 0 so any MMIO access before the next power-up fails.
  RunInEnvironment([&] { resources_.clock_regs.set_disable_result(zx::error(ZX_ERR_TIMED_OUT)); });
  ForceRuntimeSuspend();
  controller_.set_powered(false);
  EXPECT_FALSE(IsPowered());
  EXPECT_EQ(TakeLog(), (EventLog{Disabled(kBusClock)}));
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 0u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 1u);
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.ref_count(), 1u);
    EXPECT_EQ(resources_.clock_bus.ref_count(), 0u);
    resources_.clock_regs.set_disable_result(zx::ok());
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
      ASSERT_NE(cond, nullptr);
      EXPECT_EQ(cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(cond->occurrence_count(), 1u);
      EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "disable clock-registers");
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 1u);
    }
  });

  // Case 2: power back up (taking fresh votes without retrying the earlier Disable; restore
  // `controller_` power once `clock_regs` is enabled), then fail bus clock Disable on
  // SystemSuspend.
  controller_.set_powered(true);
  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 1u);
    EXPECT_EQ(resources_.clock_regs.ref_count(), 2u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 2u);
  });
  TakeLog();

  RunInEnvironment([&] { resources_.clock_bus.set_disable_result(zx::error(ZX_ERR_IO)); });
  EXPECT_OK(SystemSuspend());
  controller_.set_powered(false);
  EXPECT_FALSE(IsPowered());
  EXPECT_EQ(TakeLog(), (EventLog{Disabled(kRegistersClock)}));
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 0u);
    EXPECT_EQ(resources_.powerdomain.ref_count(), 2u);
    EXPECT_EQ(resources_.clock_bus.disable_calls(), 2u);
    EXPECT_EQ(resources_.clock_bus.ref_count(), 1u);
    resources_.clock_bus.set_disable_result(zx::ok());
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
      ASSERT_NE(cond, nullptr);
      EXPECT_EQ(cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(cond->occurrence_count(), 2u);
      EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "disable clock-bus");
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 2u);
    }
  });

  // Neither SystemSuspend nor Stop retries earlier failed releases when already unpowered.
  EXPECT_OK(SystemSuspend());
  ASSERT_NO_FATAL_FAILURE(StopDriver());
  EXPECT_EQ(TakeLog(), EventLog{});
  RunInEnvironment([&] {
    EXPECT_EQ(resources_.powerdomain.disable_calls(), 0u);
    EXPECT_EQ(resources_.clock_bus.disable_calls(), 2u);
    EXPECT_EQ(resources_.clock_regs.disable_calls(), 2u);
  });
}

// A power-up failure whose rollback succeeds latches only I2C_RESUME_FAILED; if rolling back an
// already-acquired vote also fails, both I2C_RESUME_FAILED and I2C_POWER_RELEASE_FAILED are
// latched.
TEST_F(DwI2cRuntimePmTest, PowerUpFailureAndRollbackFailureLatchHealthConditions) {
  EnableSimulatedBus();
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ForceRuntimeSuspend();
  ExpectVotes(0);

  // Clean rollback: power domain is disabled again and only I2C_RESUME_FAILED is latched.
  RunInEnvironment([&] { resources_.clock_regs.set_enable_result(zx::error(ZX_ERR_IO)); });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_IO);
  EXPECT_FALSE(IsPowered());
  ExpectVotes(0);
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      ASSERT_TRUE(res->conditions().has_value());
      EXPECT_EQ(res->conditions()->size(), 1u);
      const auto* resume_cond = FindCondition(*res, DwI2c::kReasonResumeFailed);
      ASSERT_NE(resume_cond, nullptr);
      EXPECT_EQ(resume_cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(resume_cond->occurrence_count(), 1u);
      EXPECT_EQ(FindDiagString(resume_cond->diagnostics(), DwI2c::kDiagReason),
                "enable clock-registers");
      EXPECT_EQ(FindCondition(*res, DwI2c::kReasonPowerReleaseFailed), nullptr);
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagResumeFailureCount), 1u);
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 0u);
    }
  });

  // Failed rollback: both I2C_RESUME_FAILED and I2C_POWER_RELEASE_FAILED are latched.
  RunInEnvironment([&] { resources_.powerdomain.set_disable_result(zx::error(ZX_ERR_TIMED_OUT)); });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_IO);
  EXPECT_FALSE(IsPowered());
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (auto level : {fuchsia_driver_framework::HealthCheckRequest::kPing,
                       fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      ASSERT_TRUE(res->conditions().has_value());
      EXPECT_EQ(res->conditions()->size(), 2u);
      const auto* resume_cond = FindCondition(*res, DwI2c::kReasonResumeFailed);
      ASSERT_NE(resume_cond, nullptr);
      EXPECT_EQ(resume_cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(resume_cond->occurrence_count(), 2u);
      EXPECT_EQ(FindDiagString(resume_cond->diagnostics(), DwI2c::kDiagReason),
                "enable clock-registers");
      const auto* release_cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
      ASSERT_NE(release_cond, nullptr);
      EXPECT_EQ(release_cond->status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
      EXPECT_EQ(release_cond->occurrence_count(), 1u);
      EXPECT_EQ(FindDiagString(release_cond->diagnostics(), DwI2c::kDiagReason),
                "disable power domain");
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagResumeFailureCount), 2u);
      EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 1u);
    }
  });

  // Powering up and completing a transfer clears I2C_RESUME_FAILED without clearing
  // I2C_POWER_RELEASE_FAILED; a subsequent clean power-down also leaves I2C_POWER_RELEASE_FAILED
  // latched because the leaked rollback vote is never retried.
  RunInEnvironment([&] {
    resources_.clock_regs.set_enable_result(zx::ok());
    resources_.powerdomain.set_disable_result(zx::ok());
  });
  EXPECT_OK(TransactWrite());
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    EXPECT_EQ(FindCondition(*res, DwI2c::kReasonResumeFailed), nullptr);
    EXPECT_NE(FindCondition(*res, DwI2c::kReasonPowerReleaseFailed), nullptr);
  });

  ForceRuntimeSuspend();
  RunInEnvironment([&] { EXPECT_EQ(resources_.powerdomain.ref_count(), 1u); });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    EXPECT_EQ(driver.health_latch().status(), fuchsia_driver_framework::HealthStatus::kUnhealthy);
    ASSERT_EQ(driver.health_latch().Snapshot().size(), 1u);
    EXPECT_EQ(driver.health_latch().Snapshot()[0].reason_code(), DwI2c::kReasonPowerReleaseFailed);
  });
}

TEST_F(DwI2cRuntimePmTest, InspectRuntimePm) {
  resources_.serve_irq_controller = true;
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  struct RuntimePmValues {
    bool powered;
    bool irq_controller_vote;
    uint64_t power_up_count;
    uint64_t power_down_count;
    uint64_t power_up_failures;
    uint64_t power_down_failures;
  };
  auto get_bool = [](const inspect::Hierarchy& node,
                     const std::string& name) -> std::optional<bool> {
    const auto* property = node.node().get_property<inspect::BoolPropertyValue>(name);
    if (property == nullptr) {
      ADD_FAILURE() << "missing property " << name;
      return std::nullopt;
    }
    return property->value();
  };
  auto get_uint = [](const inspect::Hierarchy& node,
                     const std::string& name) -> std::optional<uint64_t> {
    const auto* property = node.node().get_property<inspect::UintPropertyValue>(name);
    if (property == nullptr) {
      ADD_FAILURE() << "missing property " << name;
      return std::nullopt;
    }
    return property->value();
  };
  // Returns std::nullopt (after recording a failure) if the node or a property is missing.
  auto read_runtime_pm = [&]() -> std::optional<RuntimePmValues> {
    // `runtime_pm` is a lazy node, so read through the Inspector rather than its VMO.
    fpromise::result<inspect::Hierarchy> hierarchy;
    driver_test_.RunInDriverContext([&](TestDwI2c& driver) {
      hierarchy =
          fpromise::run_single_threaded(inspect::ReadFromInspector(driver.inspector_for_testing()));
    });
    if (!hierarchy.is_ok()) {
      ADD_FAILURE() << "failed to read inspect";
      return std::nullopt;
    }
    const inspect::Hierarchy* node = hierarchy.value().GetByPath({"runtime_pm"});
    if (node == nullptr) {
      ADD_FAILURE() << "missing runtime_pm node";
      return std::nullopt;
    }
    std::optional powered = get_bool(*node, "powered");
    std::optional irq_controller_vote = get_bool(*node, "irq_controller_vote");
    std::optional power_up_count = get_uint(*node, "power_up_count");
    std::optional power_down_count = get_uint(*node, "power_down_count");
    std::optional power_up_failures = get_uint(*node, "power_up_failures");
    std::optional power_down_failures = get_uint(*node, "power_down_failures");
    if (!powered || !irq_controller_vote || !power_up_count || !power_down_count ||
        !power_up_failures || !power_down_failures) {
      return std::nullopt;
    }
    return RuntimePmValues{
        .powered = *powered,
        .irq_controller_vote = *irq_controller_vote,
        .power_up_count = *power_up_count,
        .power_down_count = *power_down_count,
        .power_up_failures = *power_up_failures,
        .power_down_failures = *power_down_failures,
    };
  };

  std::optional values = read_runtime_pm();
  ASSERT_TRUE(values);
  EXPECT_TRUE(values->powered);
  EXPECT_TRUE(values->irq_controller_vote);
  EXPECT_EQ(values->power_up_count, 1u);
  EXPECT_EQ(values->power_down_count, 0u);

  ForceRuntimeSuspend();
  values = read_runtime_pm();
  ASSERT_TRUE(values);
  EXPECT_FALSE(values->powered);
  EXPECT_FALSE(values->irq_controller_vote);
  EXPECT_EQ(values->power_down_count, 1u);

  RunInEnvironment(
      [&] { resources_.irq_controller.set_enable_result(zx::error(ZX_ERR_BAD_STATE)); });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_BAD_STATE);
  values = read_runtime_pm();
  ASSERT_TRUE(values);
  EXPECT_EQ(values->power_up_failures, 1u);
  EXPECT_EQ(values->power_up_count, 1u);
  EXPECT_EQ(values->power_down_failures, 0u);

  // A power-down whose release fails is counted.
  RunInEnvironment([&] { resources_.irq_controller.set_enable_result(zx::ok()); });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  RunInEnvironment([&] { resources_.powerdomain.set_disable_result(zx::error(ZX_ERR_TIMED_OUT)); });
  ForceRuntimeSuspend();
  values = read_runtime_pm();
  ASSERT_TRUE(values);
  EXPECT_EQ(values->power_down_failures, 1u);
  EXPECT_EQ(values->power_down_count, 2u);
  RunInEnvironment([&] { resources_.powerdomain.set_disable_result(zx::ok()); });
}

// A board without a "sleep" pin state is only asked for it once, silently skipping the missing
// state on subsequent power-downs.
TEST_F(DwI2cRuntimePmTest, MissingSleepPinStateIsOnlyRequestedOnce) {
  RunInEnvironment(
      [](TestEnvironment& env) { env.pin_states().set_select_error("sleep", ZX_ERR_NOT_FOUND); });
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  for (int i = 0; i < 3; ++i) {
    ForceRuntimeSuspend();
    EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  }
  ForceRuntimeSuspend();

  RunInEnvironment([](TestEnvironment& env) {
    EXPECT_EQ(env.pin_states().select_count("sleep"), 1u);
    EXPECT_EQ(env.pin_states().select_count("default"), 4u);
  });
  ExpectVotes(0);
}

// Failing to select the default pin state or failing HostInit() aborts power-up, rolls back
// acquired votes, re-asserts reset, selects the sleep pin state, and latches I2C_RESUME_FAILED
// until a subsequent power-up succeeds.
TEST_F(DwI2cRuntimePmTest, DefaultPinStateFailureFailsPowerUpAndRollsBack) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ForceRuntimeSuspend();

  // 1. Default pin-state failure fails TransactWrite(), re-asserts reset, parks pins in "sleep",
  // and rolls back all votes.
  RunInEnvironment([](TestEnvironment& env) {
    env.reset().take_asserted();
    env.pin_states().set_select_error("default", ZX_ERR_IO);
  });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_IO);
  EXPECT_FALSE(IsPowered());
  ExpectVotes(0);
  RunInEnvironment([](TestEnvironment& env) {
    EXPECT_EQ(env.pin_states().select_count("default"), 2u);
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
    EXPECT_TRUE(env.reset().take_asserted());
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    const auto* cond = FindCondition(*res, DwI2c::kReasonResumeFailed);
    ASSERT_NE(cond, nullptr);
    EXPECT_EQ(cond->occurrence_count(), 1u);
    EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "select default pin state");
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagResumeFailureCount), 1u);
  });

  // 2. HostInit() failure (bad CompTypeReg) also rolls back votes, re-asserts reset, and selects
  // the sleep pin state.
  RunInEnvironment([](TestEnvironment& env) { env.pin_states().clear_select_error("default"); });
  controller_.set_comp_type_override(0xdeadbeef);
  EXPECT_STATUS(TransactWrite(), ZX_ERR_NOT_SUPPORTED);
  EXPECT_FALSE(IsPowered());
  ExpectVotes(0);
  RunInEnvironment([](TestEnvironment& env) {
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
    EXPECT_TRUE(env.reset().take_asserted());
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    const auto* cond = FindCondition(*res, DwI2c::kReasonResumeFailed);
    ASSERT_NE(cond, nullptr);
    EXPECT_EQ(cond->occurrence_count(), 2u);
    EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "HostInit failed");
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagResumeFailureCount), 2u);
  });

  // 3. Once power-up succeeds, I2C_RESUME_FAILED is cleared.
  controller_.set_comp_type_override(std::nullopt);
  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  EXPECT_TRUE(IsPowered());
  ExpectVotes(1);
  RunInEnvironment(
      [](TestEnvironment& env) { EXPECT_EQ(env.pin_states().last_state(), "default"); });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    EXPECT_EQ(FindCondition(*res, DwI2c::kReasonResumeFailed), nullptr);
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagResumeFailureCount), 2u);
  });
}

// Reset and non-NOT_FOUND sleep pin-state failures are counted and latched via HealthCheck while
// still rolling back or completing the remaining power-down steps.
TEST_F(DwI2cRuntimePmTest, ResetAndSleepPinStateFailuresLatchHealthConditions) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());

  // 1. A non-NOT_FOUND sleep pin-state error latches I2C_POWER_RELEASE_FAILED while still
  // asserting reset and releasing votes.
  RunInEnvironment([](TestEnvironment& env) {
    env.reset().take_asserted();
    env.pin_states().set_select_error("sleep", ZX_ERR_IO);
  });
  ForceRuntimeSuspend();
  ExpectVotes(0);
  RunInEnvironment([](TestEnvironment& env) {
    EXPECT_TRUE(env.reset().take_asserted());
    env.pin_states().clear_select_error("sleep");
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
    ASSERT_NE(cond, nullptr);
    EXPECT_EQ(cond->occurrence_count(), 1u);
    EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "select sleep pin state");
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 1u);
  });

  // 2. A reset Deassert error fails power-up, rolls back votes without asserting reset, selects
  // the sleep pin state, and latches I2C_RESUME_FAILED.
  RunInEnvironment([](TestEnvironment& env) {
    env.reset().take_asserted();
    env.reset().set_deassert_result(zx::error(ZX_ERR_IO));
  });
  EXPECT_STATUS(TransactWrite(), ZX_ERR_IO);
  EXPECT_FALSE(IsPowered());
  ExpectVotes(0);
  RunInEnvironment([](TestEnvironment& env) {
    EXPECT_FALSE(env.reset().take_asserted());
    EXPECT_EQ(env.pin_states().last_state(), "sleep");
  });
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    const auto* cond = FindCondition(*res, DwI2c::kReasonResumeFailed);
    ASSERT_NE(cond, nullptr);
    EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "deassert reset");
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagResumeFailureCount), 1u);
  });
  RunInEnvironment([](TestEnvironment& env) { env.reset().set_deassert_result(zx::ok()); });

  // 3. A reset Assert error on power-down still releases votes and updates
  // I2C_POWER_RELEASE_FAILED.
  EXPECT_STATUS(TransactWrite(), ZX_ERR_TIMED_OUT);
  RunInEnvironment(
      [](TestEnvironment& env) { env.reset().set_assert_result(zx::error(ZX_ERR_BAD_STATE)); });
  ForceRuntimeSuspend();
  ExpectVotes(0);
  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    HealthCheckResult res =
        RunHealthCheck(driver, fuchsia_driver_framework::HealthCheckRequest::kPing);
    ASSERT_TRUE(res.is_ok());
    const auto* cond = FindCondition(*res, DwI2c::kReasonPowerReleaseFailed);
    ASSERT_NE(cond, nullptr);
    EXPECT_EQ(cond->occurrence_count(), 2u);
    EXPECT_EQ(FindDiagString(cond->diagnostics(), DwI2c::kDiagReason), "assert reset");
    EXPECT_EQ(FindDiagUint(res->diagnostics(), DwI2c::kDiagPowerReleaseFailureCount), 2u);
  });
  RunInEnvironment([](TestEnvironment& env) { env.reset().set_assert_result(zx::ok()); });
}

// A health check never powers the controller up: while runtime suspended, Fast reports the same
// as Ping (with resources_enabled == false) without touching the registers.
TEST_F(DwI2cRuntimePmTest, HealthCheckDoesNotPowerUp) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  ForceRuntimeSuspend();

  driver_test_.RunInDriverContext([](TestDwI2c& driver) {
    for (fuchsia_driver_framework::HealthCheckRequest level :
         {fuchsia_driver_framework::HealthCheckRequest::kPing,
          fuchsia_driver_framework::HealthCheckRequest::kFast}) {
      HealthCheckResult res = RunHealthCheck(driver, level);
      ASSERT_TRUE(res.is_ok());
      EXPECT_TRUE(!res->conditions().has_value() || res->conditions()->empty());
      ASSERT_TRUE(res->diagnostics().has_value());
      bool found_resources_enabled = false;
      for (const auto& item : *res->diagnostics()) {
        EXPECT_NE(item.key(), DwI2c::kDiagCompType);
        if (item.key() == DwI2c::kDiagResourcesEnabled) {
          EXPECT_FALSE(item.value().bool_value().value());
          found_resources_enabled = true;
        }
      }
      EXPECT_TRUE(found_resources_enabled);
    }
  });
  EXPECT_FALSE(IsPowered());
  ExpectVotes(0);
  RunInEnvironment([&] { EXPECT_EQ(resources_.powerdomain.enable_calls(), 1u); });
}

}  // namespace dw_i2c
