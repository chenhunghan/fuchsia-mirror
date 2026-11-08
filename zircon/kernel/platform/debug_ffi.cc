// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/page/size.h>
#include <lib/uart/all.h>
#include <lib/uart/null.h>
#include <lib/uart/uart.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <platform.h>
#include <stdio.h>

#include <cassert>
#include <type_traits>

#include <arch/arch_interrupt.h>
#include <dev/init.h>
#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/lockdep.h>
#include <kernel/spinlock.h>
#include <ktl/optional.h>
#include <ktl/type_traits.h>
#include <lockdep/guard.h>
#include <platform/debug.h>
#include <platform/uart.h>

extern "C" {

bool rust_uart_is_tx_irq_enabled();
void rust_uart_irq_provider_set_interrupts_enabled(uint32_t irq_num, bool enabled);
bool rust_uart_rx_queue_full();
void rust_uart_rx_queue_write_char(uint8_t c);
void rust_uart_driver_handoff_early(const uart::all::Driver* serial);
void rust_uart_driver_handoff_post_vm(const uart::all::Driver* serial);
void rust_uart_driver_handoff_late(const uart::all::Driver* serial);

enum class UartDriverKind : uint32_t {
  kNull = 0,
  kMmio = 1,
  kPio = 2,
};

struct UartDriverInfo {
  UartDriverKind kind;
  bool has_irq;
  uint32_t irq;
  bool has_simple_cfg;
  uint32_t cfg_flags;
  size_t io_base;
  size_t io_size;
};

static_assert(sizeof(UartDriverKind) == 4);
static_assert(alignof(UartDriverKind) == 4);
static_assert(sizeof(UartDriverInfo) == 40);
static_assert(alignof(UartDriverInfo) == 8);

}  // extern "C"

namespace {

// No locks will be acquired.
struct NullLockPolicy {};

template <typename LockPolicy, typename Guard>
using GuardSelector =
    std::conditional_t<std::is_same_v<LockPolicy, NullLockPolicy>, NullGuard, Guard>;

// Implements SyncPolicy as defined in <lib/uart/sync.h>
struct UartSyncPolicy {
  // While tempting, do *not* use a templated `using` statement here.  In some
  // places in code, we will need to deduce (via argument passing) what
  // `MemberOf` is.  Attempting to do this when we declare lock as a using alias
  // is not going to work in this situation.  The deduction is going to execute
  // against the underlying type, not the `using` statement.  So the final type
  // (in this case, a LockDep instance) would need to be a template with exactly
  // one argument which is identical to MemberOf.  In the case of LockDep locks,
  // this is *not* the case.
  //
  // Declaring this as a single-argument templated struct with no members which
  // inherits from the underlying LockDep type works around this issue.  Guards
  // and other things will auto-upcast to the proper underlying LockDep type,
  // and the only constructor needed/supported is the default constructor, so we
  // don't need to write a forwarding constructor to the underlying type.
  template <typename MemberOf>
  struct Lock : public DECLARE_SPINLOCK_WITH_TYPE(MemberOf, MonitoredSpinLock) {};

  template <typename LockPolicy>
  using Guard = GuardSelector<LockPolicy, Guard<MonitoredSpinLock, LockPolicy>>;

  class Waiter {
   public:
    enum class Blocking {
      // `Wait` is allowed to block callers, e.g. wait on an event.
      kYes,
      // `Wait` is not allowed to block callers, should spin instead.
      kNo,
    };

    template <typename Guard, typename T>
    void Wait(Guard& guard, T&& enable_tx_interrupt, Blocking blocking) TA_REQ(guard) {
      if (blocking == Blocking::kYes && rust_uart_is_tx_irq_enabled()) {
        enable_tx_interrupt();
        guard.CallUnlocked([this]() { tx_fifo_not_full_.Wait(); });
      } else {
        // Drop the spinlock while spinning.
        guard.CallUnlocked([]() { arch::Yield(); });
      }
    }

    void Wake() { tx_fifo_not_full_.Signal(); }

   private:
    AutounsignalEvent tx_fifo_not_full_{true};
  };

  using DefaultLockPolicy = NullLockPolicy;

  template <typename LockType>
  static void AssertHeld(LockType& lock) TA_ASSERT(lock) TA_ASSERT(lock.lock()) {
    lock.lock().AssertHeld();
  }
};

class KernelIrqProvider {
 public:
  KernelIrqProvider() = default;
  ~KernelIrqProvider() = default;

  // No copy, no move.
  KernelIrqProvider(const KernelIrqProvider&) = delete;
  KernelIrqProvider& operator=(const KernelIrqProvider&) = delete;
  KernelIrqProvider(KernelIrqProvider&&) = delete;
  KernelIrqProvider& operator=(KernelIrqProvider&&) = delete;

  void Init(int irq_num) { irq_num_ = irq_num; }
  void SetInterruptsEnabled(bool enabled) const {
    rust_uart_irq_provider_set_interrupts_enabled(static_cast<uint32_t>(irq_num_), enabled);
  }

 private:
  int irq_num_{0};
};

uart::all::KernelDriver<PlatformUartIoProvider, UartSyncPolicy, KernelIrqProvider> gUart;

size_t UnparseUart(char* buf, size_t len) {
  StringFile file{{buf, len}};
  gUart.Unparse(&file);
  return file.as_string_view().size();
}

void GetUartDriverInfo(UartDriverInfo* out) {
  gUart.Visit([out]<typename DriverType>(DriverType& driver) {
    using uart_type = DriverType::uart_type;
    using cfg_type = uart_type::config_type;
    if constexpr (ktl::is_same_v<cfg_type, zbi_dcfg_simple_t>) {
      out->has_simple_cfg = true;
      out->cfg_flags = driver.config().flags;
    } else {
      out->has_simple_cfg = false;
      out->cfg_flags = 0;
    }
    if constexpr (uart::MmioDriver<uart_type>) {
      out->kind = UartDriverKind::kMmio;
      ktl::optional<uint32_t> irq = PlatformUartGetIrqNumber(driver.config().irq);
      out->has_irq = irq.has_value();
      out->irq = irq.value_or(0);
      uart::MmioRange aligned_mmio_range = driver.mmio_range().AlignedTo(kPageSize);
      out->io_base = aligned_mmio_range.address;
      out->io_size = aligned_mmio_range.size;
    } else if constexpr (uart::PioDriver<uart_type>) {
      out->kind = UartDriverKind::kPio;
      ktl::optional<uint32_t> irq = PlatformUartGetIrqNumber(driver.config().irq);
      out->has_irq = irq.has_value();
      out->irq = irq.value_or(0);
      uart::IoPortRange ioports = driver.ioports();
      out->io_base = ioports.base;
      out->io_size = ioports.count;
    } else {  // Only |uart::null::Driver| is expected to have a different configuration type.
      constexpr auto kIsNullDriver = ktl::is_same_v<uart_type, uart::null::Driver>;
      ZX_ASSERT_MSG(kIsNullDriver, "Unexpected UART Configuration.");
      out->kind = UartDriverKind::kNull;
      out->has_irq = false;
      out->irq = 0;
      out->io_base = 0;
      out->io_size = 0;
    }
  });
}

void InitUartInterrupt(uint32_t uart_irq) {
  gUart.Visit([uart_irq](auto& driver) { driver.InitInterrupt(uart_irq); });
}

void HandleUartInterrupt() {
  static constexpr auto rx_irq_handler = [](auto& rx_interrupt) {
    // This check needs to be performed under a lock, such that we prevent operation
    // interleaving that would leave us in a blocked state.
    //
    // E.g.
    // Assume a simple MT scenario with one reader R and one writer R:
    //
    // * W: Observes the buffer is full.
    // * R: Reads a character. The buffer is now empty.
    // * R: Unmasks RX.
    // * W: Masks RX.
    //
    //  At this point, we have an empty buffer and RX interrupts are masked -
    //  we're stuck! Thus, to avoid this, we acquire the spinlock before
    //  checking if the buffer is full, and release after (conditionally)
    //  masking RX interrupts. By pairing this with the acquisition of the
    //  same lock around unmasking RX interrupts, we prevent the writer above
    //  from being interrupted by a read-and-unmask.
    uint8_t c;
    {
      Guard<MonitoredSpinLock, NoIrqSave> lock(&rx_interrupt.lock(), SOURCE_TAG);
      if (rust_uart_rx_queue_full()) {
        // disables RX interrupts.
        rx_interrupt.DisableInterrupt();
        return;
      }
      c = static_cast<uint8_t>(rx_interrupt.ReadChar());
    }
    rust_uart_rx_queue_write_char(c);
  };

  static constexpr auto tx_irq_handler = [](auto& tx_interrupt) {
    // Mask the TX interrupt before signalling any blocked thread as there may
    // be a race between masking TX here below and unmasking by the blocked
    // thread.
    {
      Guard<MonitoredSpinLock, NoIrqSave> lock(&tx_interrupt.lock(), SOURCE_TAG);
      tx_interrupt.DisableInterrupt();
    }

    // Do not signal the event while holding the sync capability, this could lead
    // to invalid lock dependencies.
    tx_interrupt.Notify();
  };

  gUart.Visit([](auto& driver) { driver.Interrupt(tx_irq_handler, rx_irq_handler); });
}

void EnableUartRxInterrupt() {
  gUart.Visit([](auto& driver) { driver.template EnableRxInterrupt<IrqSave>(); });
}

bool IsUartTxDrained() {
  bool drained = true;
  gUart.Visit([&drained](auto& driver) { drained = driver.template TxDrained<IrqSave>(); });
  return drained;
}

void PrepareUartForSuspend() {
  gUart.Visit([](auto& driver) { driver.template PrepareForSuspend<IrqSave>(); });
}

void WakeupUartFromSuspend() {
  gUart.Visit([](auto& driver) { driver.template WakeupFromSuspend<IrqSave>(); });
}

bool ReadUartChar(uint8_t* out_c) {
  ktl::optional<char> read;
  gUart.Visit([&](auto& driver) { read = driver.Read(); });
  if (read) {
    *out_c = static_cast<uint8_t>(*read);
    return true;
  }
  return false;
}

void WriteUart(const char* str, size_t len, bool blocking) {
  const auto mode =
      blocking ? UartSyncPolicy::Waiter::Blocking::kYes : UartSyncPolicy::Waiter::Blocking::kNo;
  gUart.Visit([str, len, mode](auto& driver) { driver.template Write<IrqSave>({str, len}, mode); });
}

void WriteUartPanicChar(char c) {
  gUart.Visit([c](auto& driver) { driver.Write({&c, 1}, UartSyncPolicy::Waiter::Blocking::kNo); });
}

}  // namespace

void UartDriverHandoffEarly(const uart::all::Driver& serial) {
  rust_uart_driver_handoff_early(&serial);
}

void UartDriverHandoffPostVm(const uart::all::Driver& serial) {
  rust_uart_driver_handoff_post_vm(&serial);
}

void UartDriverHandoffLate(const uart::all::Driver& serial) {
  rust_uart_driver_handoff_late(&serial);
}

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_uart_enabled() { return static_cast<bool>(gUart); }

FFI_ALWAYS_INLINE void cpp_uart_set_driver(const void* serial) {
  gUart = *static_cast<const uart::all::Driver*>(serial);
}

FFI_ALWAYS_INLINE size_t cpp_uart_unparse(char* buf, size_t len) { return UnparseUart(buf, len); }

FFI_ALWAYS_INLINE void cpp_uart_get_driver_info(UartDriverInfo* out) { GetUartDriverInfo(out); }

FFI_ALWAYS_INLINE void cpp_uart_init_interrupt(uint32_t uart_irq) { InitUartInterrupt(uart_irq); }

FFI_ALWAYS_INLINE void cpp_uart_interrupt() { HandleUartInterrupt(); }

FFI_ALWAYS_INLINE void cpp_uart_enable_rx_interrupt() { EnableUartRxInterrupt(); }

FFI_ALWAYS_INLINE bool cpp_uart_tx_drained() { return IsUartTxDrained(); }

FFI_ALWAYS_INLINE void cpp_uart_prepare_for_suspend() { PrepareUartForSuspend(); }

FFI_ALWAYS_INLINE void cpp_uart_wakeup_from_suspend() { WakeupUartFromSuspend(); }

FFI_ALWAYS_INLINE bool cpp_uart_read(uint8_t* out_c) { return ReadUartChar(out_c); }

FFI_ALWAYS_INLINE void cpp_uart_write(const char* str, size_t len, bool blocking) {
  WriteUart(str, len, blocking);
}

FFI_ALWAYS_INLINE void cpp_uart_pputc(char c) { WriteUartPanicChar(c); }

}  // extern "C"
