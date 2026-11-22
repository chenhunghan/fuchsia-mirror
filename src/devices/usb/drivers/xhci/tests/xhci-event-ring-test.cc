// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/xhci-event-ring.h"

#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/async/cpp/task.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>

#include <atomic>
#include <thread>
#include <vector>

#include <fake-dma-buffer/fake-dma-buffer.h>
#include <gtest/gtest.h>
#include <usb/usb.h>

#include "src/devices/usb/drivers/xhci/tests/test-env.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_xhci {

constexpr size_t kErstMax = 42;
constexpr size_t kShortTransferLength0 = 97;
constexpr size_t kShortTransferLength1 = 102;
constexpr size_t kFinalTransferLength = 87;
constexpr size_t kShortTransferLength = 800;
constexpr size_t kFakeTrb = 0x3924ff0913;
constexpr size_t kFakeTrbVirt = 0x8411487132;
constexpr auto kPortNo = 5;
constexpr auto kSecondPortNo = 6;

enum class CommandType {
  EnumerateDevice,
  ResetEndpoint,
};

struct Command : public fbl::DoublyLinkedListable<std::unique_ptr<Command>> {
  uint8_t port;
  std::optional<HubInfo> hub;
  fpromise::completer<TRB*, zx_status_t> completer;
  CommandType type;
  uint8_t endpoint;
  uint32_t device_id;
};

class InterrupterTestPeer {
 public:
  [[nodiscard]] static zx_status_t Flush(Interrupter& interrupter) {
    while (true) {
      std::shared_ptr<libsync::Completion> wait_completion;
      bool retry_after_wait = false;
      zx_status_t status = ZX_OK;
      {
        fbl::AutoLock _(&interrupter.dispatcher_lock_);
        if (auto* starting =
                std::get_if<Interrupter::StartingState>(&interrupter.dispatcher_state_)) {
          wait_completion = starting->completion;
          retry_after_wait = true;
        } else if (auto* stopping =
                       std::get_if<Interrupter::StoppingState>(&interrupter.dispatcher_state_)) {
          wait_completion = stopping->completion;
        } else if (auto* running =
                       std::get_if<Interrupter::RunningState>(&interrupter.dispatcher_state_)) {
          wait_completion = std::make_shared<libsync::Completion>();
          status = async::PostTask(
              running->dispatcher.async_dispatcher(),
              [signal = fit::defer([wait_completion] { wait_completion->Signal(); })] {});
        } else {
          return ZX_OK;
        }
      }
      if (status != ZX_OK) {
        return status;
      }
      wait_completion->Wait();
      if (!retry_after_wait) {
        return ZX_OK;
      }
    }
  }

  static bool StartingStopRequested(Interrupter& interrupter) {
    fbl::AutoLock _(&interrupter.dispatcher_lock_);
    auto* starting = std::get_if<Interrupter::StartingState>(&interrupter.dispatcher_state_);
    return starting != nullptr && starting->stop_requested;
  }

  static bool IsStopping(Interrupter& interrupter) {
    fbl::AutoLock _(&interrupter.dispatcher_lock_);
    return std::holds_alternative<Interrupter::StoppingState>(interrupter.dispatcher_state_);
  }

  static bool HasStoppingWaiter(Interrupter& interrupter) {
    fbl::AutoLock _(&interrupter.dispatcher_lock_);
    auto* stopping = std::get_if<Interrupter::StoppingState>(&interrupter.dispatcher_state_);
    // The Stop() owner holds my_stop_completion and StoppingState holds completion (2 refs);
    // any concurrent Stop() waiter holds an additional reference (> 2).
    return stopping != nullptr && stopping->completion.use_count() > 2;
  }

  // Runs irq_handler_.Cancel() on the interrupter's synchronized dispatcher. Returns
  // ZX_ERR_NOT_FOUND if irq_handler_ was already canceled, or ZX_OK if it was still pending.
  [[nodiscard]] static zx_status_t CancelIrqHandler(Interrupter& interrupter) {
    std::atomic<zx_status_t> cancel_status{ZX_ERR_INTERNAL};
    auto done = std::make_shared<libsync::Completion>();
    zx_status_t post_status = ZX_ERR_BAD_STATE;
    {
      fbl::AutoLock _(&interrupter.dispatcher_lock_);
      if (auto* running = std::get_if<Interrupter::RunningState>(&interrupter.dispatcher_state_)) {
        post_status = async::PostTask(
            running->dispatcher.async_dispatcher(),
            [&interrupter, &cancel_status, signal = fit::defer([done] { done->Signal(); })] {
              cancel_status.store(interrupter.irq_handler_.Cancel(), std::memory_order_release);
            });
      }
    }
    if (post_status != ZX_OK) {
      return post_status;
    }
    done->Wait();
    return cancel_status.load(std::memory_order_acquire);
  }
};

namespace {

class FakeActivityGovernor
    : public fidl::testing::TestBase<fuchsia_power_system::ActivityGovernor> {
 public:
  fidl::ProtocolHandler<fuchsia_power_system::ActivityGovernor> CreateHandler(
      async_dispatcher_t* dispatcher) {
    return [this, dispatcher](fidl::ServerEnd<fuchsia_power_system::ActivityGovernor> server_end) {
      connect_count_.fetch_add(1, std::memory_order_release);
      bindings_.AddBinding(dispatcher, std::move(server_end), this, fidl::kIgnoreBindingClosure);
    };
  }

  void RegisterSuspendBlocker(RegisterSuspendBlockerRequest& request,
                              RegisterSuspendBlockerCompleter::Sync& completer) override {
    if (auto& blocker = request.suspend_blocker(); blocker.has_value()) {
      suspend_blockers_.push_back(std::move(*blocker));
    }
    register_count_.fetch_add(1, std::memory_order_release);
    zx::eventpair client_token;
    zx::eventpair server_token;
    if (zx_status_t status = zx::eventpair::create(0, &client_token, &server_token);
        status != ZX_OK) {
      ADD_FAILURE() << "zx::eventpair::create failed: " << zx_status_get_string(status);
      completer.Close(status);
      return;
    }
    fuchsia_power_system::ActivityGovernorRegisterSuspendBlockerResponse response;
    response.token() = std::move(client_token);
    completer.Reply(fit::ok(std::move(response)));
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << name << " is not implemented";
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_system::ActivityGovernor> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

  size_t connect_count() const { return connect_count_.load(std::memory_order_acquire); }
  size_t register_count() const { return register_count_.load(std::memory_order_acquire); }

  std::vector<fidl::ClientEnd<fuchsia_power_system::SuspendBlocker>> TakeSuspendBlockers() {
    std::vector<fidl::ClientEnd<fuchsia_power_system::SuspendBlocker>> out;
    out.swap(suspend_blockers_);
    return out;
  }

 private:
  std::atomic<size_t> connect_count_{0};
  std::atomic<size_t> register_count_{0};
  std::vector<fidl::ClientEnd<fuchsia_power_system::SuspendBlocker>> suspend_blockers_;
  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> bindings_;
};

class EventRingTestEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    return to_driver_vfs.component().AddUnmanagedProtocol<fuchsia_power_system::ActivityGovernor>(
        activity_governor_.CreateHandler(dispatcher));
  }

  FakeActivityGovernor& activity_governor() { return activity_governor_; }

 private:
  FakeActivityGovernor activity_governor_;
};

class EventRingTestConfig final {
 public:
  using DriverType = usb_xhci::UsbXhci;
  using EnvironmentType = EventRingTestEnvironment;
};

}  // namespace

class EventRingHarness : public ::testing::Test {
 public:
  EventRingHarness() : trb_context_allocator_(-1, true) {}
  void SetUp() override {
    // Globals
    constexpr auto kRuntimeRegisterOffset = 6 * sizeof(uint32_t);
    constexpr auto kErdp = 2062 * sizeof(uint32_t);
    region_.emplace(sizeof(uint32_t), 4096);
    buffer_.emplace(region_->GetMmioBuffer());

    controller_region_.emplace(sizeof(uint32_t), 4);
    (*controller_region_)[0].SetReadCallback([this] {
      auto cmd = USBCMD::Get(0).FromValue(0);
      cmd.set_ENABLE(controller_enabled_ ? 1 : 0);
      return cmd.reg_value();
    });
    (*controller_region_)[sizeof(uint32_t)].SetReadCallback([this] {
      auto sts = USBSTS::Get(0).FromValue(0);
      sts.set_HCHalted(controller_halted_ ? 1 : 0);
      return sts.reg_value();
    });

    // Core registers
    (*region_)[kRuntimeRegisterOffset].SetReadCallback([=]() { return 0x2000; });
    (*region_)[kErdp].SetReadCallback([=]() { return erdp_; });
    (*region_)[kErdp].SetWriteCallback([=](uint64_t value) {
      ERDP reg;
      reg.set_reg_value(value);
      erdp_ = reg.Pointer();
      last_erdp_ = reg;
    });

    // Port/per-slot registers
    (*region_)[PORTSC::Get(0, kPortNo).addr()].SetReadCallback([=]() {
      port_sc_read_count_++;
      if (port_sc_read_hook_) {
        (*port_sc_read_hook_)();
      }
      return port_sc_.reg_value();
    });
    (*region_)[PORTSC::Get(0, kPortNo).addr()].SetWriteCallback([=](uint64_t value) {
      port_sc_write_count_++;
      PORTSC sc;
      sc.set_reg_value(static_cast<uint32_t>(value));
      if (sc.PR()) {
        port_sc_.set_PR(true);
      }
      if (sc.CSC()) {
        port_sc_.set_CSC(false);
      }
      if (sc.PRC()) {
        port_sc_.set_PRC(false);
      }
      if (sc.PLC()) {
        port_sc_.set_PLC(false);
      }
      if (sc.CEC()) {
        port_sc_.set_CEC(false);
      }
      if (port_sc_write_hook_) {
        (*port_sc_write_hook_)(sc);
      }
    });
    (*region_)[PORTSC::Get(0, kSecondPortNo).addr()].SetReadCallback(
        [=]() { return second_port_sc_.reg_value(); });
    (*region_)[PORTSC::Get(0, kSecondPortNo).addr()].SetWriteCallback([=](uint64_t value) {
      PORTSC sc;
      sc.set_reg_value(static_cast<uint32_t>(value));
      if (sc.CSC()) {
        second_port_sc_.set_CSC(false);
      }
      if (sc.PRC()) {
        second_port_sc_.set_PRC(false);
      }
      if (sc.PLC()) {
        second_port_sc_.set_PLC(false);
      }
      if (sc.CEC()) {
        second_port_sc_.set_CEC(false);
      }
    });
    ASSERT_TRUE(driver_test()
                    .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                      xhci_config::Config fake_config;
                      fake_config.enable_suspend() = enable_suspend();
                      args.config(fake_config.ToVmo());
                    })
                    .is_ok());
    ASSERT_OK(driver_test().driver()->TestInit(this));
  }

  void TearDown() override { ASSERT_TRUE(driver_test().StopDriver().is_ok()); }

  void Interrupt() { ring_->HandleIRQ(); }

  zx_status_t Ring0Bringup() { return ring_->Ring0Bringup(); }

  void AddTRB(const TRB& trb) {
    auto ptr = ddk_fake::PhysToVirt<TRB*>(erdp_);
    *ptr = trb;
    Control::FromTRB(ptr).set_Cycle(1).ToTrb(ptr);
    erdp_ += sizeof(TRB);
  }

  void ConnectDevice() {
    port_sc_.set_CCS(true);
    port_sc_.set_PED(true);
    port_sc_.set_PR(false);
    port_sc_.set_PLS(PORTSC::Polling);
    port_sc_.set_CSC(true);
  }

  void DisconnectDevice() {
    port_sc_.set_CCS(false);
    port_sc_.set_CSC(true);
  }

  void MakeQuantumPort() {
    // Port is enabled and disabled simultaneously
    port_sc_.set_PLS(PORTSC::Disabled);
    port_sc_.set_PED(1);
  }

  void LinkUp() {
    port_sc_.set_PR(false);
    port_sc_.set_PLS(PORTSC::U0);
  }

  bool PortResetPending() { return port_sc_.PR(); }

  void InsertPortStatusChangeEvent(uint8_t port_id = kPortNo) {
    TRB trb;
    trb.ptr = 0;
    Control::FromTRB(&trb).set_Type(Control::PortStatusChangeEvent).ToTrb(&trb);
    auto evt = static_cast<PortStatusChangeEvent*>(&trb);
    evt->set_PortID(port_id);
    auto command_completion = static_cast<CommandCompletionEvent*>(&trb);
    command_completion->set_CompletionCode(CommandCompletionEvent::Success);
    AddTRB(trb);
  }

  TRB* trb() const { return ddk_fake::PhysToVirt<TRB*>(erdp_); }

  const ERDP& last_erdp() const { return last_erdp_; }

  using TestRequest = usb::CallbackRequest<sizeof(max_align_t)>;
  template <typename Callback>
  zx_status_t AllocateRequest(std::optional<TestRequest>* request, uint32_t device_id,
                              uint64_t data_size, uint8_t endpoint, Callback callback) {
    zx_status_t status =
        TestRequest::Alloc(request, data_size, endpoint,
                           driver_test().driver()->UsbHciGetRequestSize(), std::move(callback));
    if (status != ZX_OK) {
      return status;
    }
    request->value().request()->header.device_id = device_id;
    return ZX_OK;
  }

  std::unique_ptr<TRBContext> AllocateContext() { return trb_context_allocator_.New(); }

  void RequestQueue(usb_request_t* usb_request,
                    const usb_request_complete_callback_t* complete_cb) {
    pending_req_ = Request(usb_request, *complete_cb, sizeof(usb_request_t));
  }

  Request Borrow(TestRequest request) {
    static const usb_protocol_ops_t ops = {
        .request_queue =
            [](void* ctx, usb_request_t* usb_request,
               const usb_request_complete_callback_t* complete_cb) {
              auto* harness = static_cast<EventRingHarness*>(ctx);
              harness->RequestQueue(usb_request, complete_cb);
            },
    };
    usb_protocol_t proto = {
        .ops = &ops,
        .ctx = this,
    };
    ddk::UsbProtocolClient client(&proto);
    TestRequest::Queue(std::move(request), client);
    return std::move(*pending_req_);
  }

  zx_status_t InitRing(EventRing* ring) {
    if (!buffer_.has_value()) {
      return ZX_ERR_BAD_STATE;
    }
    fdf::MmioBuffer& buffer = buffer_.value();
    ring_ = ring;
    zx_status_t status = command_ring_.TransferRing::Init(zx_system_get_page_size(), kFakeBti, ring,
                                                          false, &buffer, driver_test().driver());
    if (status != ZX_OK) {
      return status;
    }
    auto regoffset = RuntimeRegisterOffset::Get().ReadFrom(&buffer);
    status = ring->Init(
        zx_system_get_page_size(), kFakeBti, &buffer, false, kErstMax,
        ERSTSZ::Get(regoffset, 0).ReadFrom(&buffer), ERDP::Get(regoffset, 0).ReadFrom(&buffer),
        IMAN::Get(regoffset, 0).ReadFrom(&buffer), CapLength::Get().ReadFrom(&buffer).Length(),
        HCSPARAMS1::Get().ReadFrom(&buffer), &command_ring_,
        DoorbellOffset::Get().ReadFrom(&buffer), driver_test().driver(),
        HCCPARAMS1::Get().ReadFrom(&buffer), dcbaa_, 0, nullptr);
    if (status != ZX_OK) {
      return status;
    }
    status = ring_->AddSegmentIfNoneLock();
    if (status != ZX_OK) {
      return status;
    }
    ERDP reg;
    reg.set_Pointer(ddk_fake::PhysToVirt<zx_paddr_t*>(ring_->erst())[0]);
    (*region_)[2062 * sizeof(uint32_t)].Write(reg.reg_value());
    return ZX_OK;
  }
  void SetShortPacketHandler(
      fit::function<zx_status_t(usb_xhci::TRB*, size_t, usb_xhci::TRB**)> handler) {
    short_packet_handler_ = std::move(handler);
  }

  zx_status_t HandleShortPacket(TRB* short_trb, size_t short_length, TRB** first_trb) {
    if (short_packet_handler_.has_value()) {
      return (*short_packet_handler_)(short_trb, short_length, first_trb);
    }
    if (pending_contexts_.is_empty()) {
      return ZX_ERR_IO;
    }
    return ZX_OK;
  }

  zx_status_t CompleteTRB(TRB* trb, std::unique_ptr<TRBContext>* context) {
    if (pending_contexts_.is_empty()) {
      return ZX_ERR_CANCELED;
    }
    if (trb != expected_completion_) {
      return ZX_ERR_IO;
    }
    *context = pending_contexts_.pop_front();
    return ZX_OK;
  }

  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TakePendingTRBs() {
    return std::move(pending_contexts_);
  }

  void SetCompletion(TRB* expected) { expected_completion_ = expected; }

  void AddContext(std::unique_ptr<TRBContext> context) {
    pending_contexts_.push_back(std::move(context));
  }

  void AddCommand(std::unique_ptr<Command> command) { commands_.push_back(std::move(command)); }

  std::unique_ptr<Command> GetCommand() { return commands_.pop_front(); }

  bool HasCommand() { return !commands_.is_empty(); }

  void InitSlot(uint8_t i) {
    // Enable SlotID i for testing with arbitrary port 1.
    auto state = driver_test().driver()->GetDeviceState(i - 1);
    ASSERT_TRUE(state);
    fbl::AutoLock _(&state->transaction_lock());
    state->SetDeviceInformation(i, 1, std::nullopt);
  }

  fdf::MmioBuffer CreateControllerMmio() {
    ZX_ASSERT(controller_region_.has_value());
    return controller_region_->GetMmioBuffer();
  }
  void SetControllerEnabled(bool enabled) { controller_enabled_ = enabled; }
  void SetControllerHalted(bool halted) { controller_halted_ = halted; }
  void RecordShutdown() { shutdown_called_ = true; }
  bool shutdown_called() const { return shutdown_called_; }

  fdf_testing::ForegroundDriverTest<EventRingTestConfig>& driver_test() { return driver_test_; }
  PORTSC& port_sc() { return port_sc_; }
  PORTSC& second_port_sc() { return second_port_sc_; }
  size_t port_sc_write_count() const { return port_sc_write_count_; }
  size_t port_sc_read_count() const { return port_sc_read_count_; }
  void SetPortScWriteHook(fit::function<void(const PORTSC&)> hook) {
    port_sc_write_hook_ = std::move(hook);
  }
  void SetPortScReadHook(fit::function<void()> hook) { port_sc_read_hook_ = std::move(hook); }
  EventRing* event_ring() { return ring_; }
  fake_mmio::FakeMmioRegRegion& mmio_region() {
    ZX_ASSERT(region_.has_value());
    return *region_;
  }
  fdf::MmioBuffer* mmio_buffer() {
    ZX_ASSERT(buffer_.has_value());
    return &buffer_.value();
  }
  uint64_t* dcbaa() { return dcbaa_; }

  struct TimeoutResult {
    std::atomic<zx_status_t> status{ZX_ERR_INTERNAL};
    libsync::Completion done;
  };

  static std::shared_ptr<TimeoutResult> ScheduleTimeout(Interrupter& interrupter, EventRing* ring,
                                                        zx::time deadline) {
    auto res = std::make_shared<TimeoutResult>();
    ring->ScheduleTask(interrupter.Timeout(deadline)
                           .then([res](fpromise::result<void, zx_status_t>& r)
                                     -> fpromise::result<void, zx_status_t> {
                             res->status.store(r.is_ok() ? ZX_OK : r.error(),
                                               std::memory_order_release);
                             res->done.Signal();
                             return fpromise::ok();
                           })
                           .box());
    return res;
  }

  zx_status_t RunImmediateTimeout(Interrupter& interrupter, zx::time deadline) {
    auto res = ScheduleTimeout(interrupter, event_ring(), deadline);
    event_ring()->RunUntilIdle();
    EXPECT_TRUE(res->done.signaled());
    return res->status.load(std::memory_order_acquire);
  }

  size_t ActivityGovernorConnectCount() {
    return driver_test().RunInEnvironmentTypeContext<size_t>(
        [](EventRingTestEnvironment& env) { return env.activity_governor().connect_count(); });
  }

  size_t ActivityGovernorRegisterCount() {
    return driver_test().RunInEnvironmentTypeContext<size_t>(
        [](EventRingTestEnvironment& env) { return env.activity_governor().register_count(); });
  }

  std::vector<fidl::ClientEnd<fuchsia_power_system::SuspendBlocker>> TakeSuspendBlockers() {
    return driver_test()
        .RunInEnvironmentTypeContext<
            std::vector<fidl::ClientEnd<fuchsia_power_system::SuspendBlocker>>>(
            [](EventRingTestEnvironment& env) {
              return env.activity_governor().TakeSuspendBlockers();
            });
  }

 protected:
  virtual bool enable_suspend() const { return false; }

 private:
  fdf_testing::ForegroundDriverTest<EventRingTestConfig> driver_test_;
  std::optional<Request> pending_req_;
  std::optional<fit::function<zx_status_t(usb_xhci::TRB*, size_t, usb_xhci::TRB**)>>
      short_packet_handler_;
  std::optional<fit::function<void(const PORTSC&)>> port_sc_write_hook_;
  std::optional<fit::function<void()>> port_sc_read_hook_;
  size_t port_sc_write_count_ = 0;
  size_t port_sc_read_count_ = 0;
  using AllocatorTraits = fbl::InstancedSlabAllocatorTraits<std::unique_ptr<TRBContext>, 4096U>;
  using AllocatorType = fbl::SlabAllocator<AllocatorTraits>;
  AllocatorType trb_context_allocator_;
  TRB* expected_completion_ = nullptr;
  fbl::DoublyLinkedList<std::unique_ptr<Command>> commands_;
  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> pending_contexts_;
  std::optional<fdf::MmioBuffer> buffer_;
  EventRing* ring_;
  PORTSC port_sc_;
  PORTSC second_port_sc_;
  CommandRing command_ring_;
  uint64_t dcbaa_[128];
  uint64_t erdp_;
  ERDP last_erdp_;
  std::optional<fake_mmio::FakeMmioRegRegion> region_;
  std::optional<fake_mmio::FakeMmioRegRegion> controller_region_;
  bool controller_enabled_ = true;
  bool controller_halted_ = false;
  bool shutdown_called_ = false;
};

// UsbXhci Methods
bool UsbXhci::Running() const { return running_; }

zx::result<> UsbXhci::Init(std::unique_ptr<dma_buffer::BufferFactory> buffer_factory) {
  buffer_factory_ = std::move(buffer_factory);

  fbl::AllocChecker ac;
  interrupters_ = fbl::MakeArray<Interrupter>(&ac, 2);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  max_slots_ = 32;
  auto device_states = fbl::MakeArray<fbl::RefPtr<DeviceState>>(&ac, max_slots_);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  for (size_t i = 0; i < max_slots_; i++) {
    device_states[i] = fbl::MakeRefCounted<DeviceState>(static_cast<uint32_t>(i), this);
    fbl::AutoLock l(&device_states[i]->transaction_lock());
    for (size_t c = 0; c < max_slots_; c++) {
      device_states[i]->InitEndpoint(static_cast<uint8_t>(c), nullptr, nullptr);
    }
  }
  InitDeviceState(std::move(device_states));
  port_state_ = fbl::MakeArray<PortState>(&ac, 32);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  params_.set_MaxPorts(32);
  cap_length_ = 0;
  mmio_.emplace(GetTestHarness<EventRingHarness>()->CreateControllerMmio());
  zx_status_t status = GetTestHarness<EventRingHarness>()->InitRing(&interrupters_[0].ring());
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok();
}

fpromise::promise<void, zx_status_t> UsbXhci::UsbHciResetEndpointAsync(uint32_t device_id,
                                                                       uint8_t ep_address) {
  auto harness = GetTestHarness<EventRingHarness>();
  std::unique_ptr<Command> command = std::make_unique<Command>();
  fpromise::bridge<TRB*, zx_status_t> bridge;
  command->type = CommandType::ResetEndpoint;
  command->device_id = device_id;
  command->endpoint = ep_address;
  command->completer = std::move(bridge.completer);
  harness->AddCommand(std::move(command));
  return bridge.consumer.promise().discard_value();
}

void UsbXhci::UsbHciRequestQueue(usb_request_t* usb_request,
                                 const usb_request_complete_callback_t* complete_cb) {}

zx_status_t UsbXhci::UsbHciCancelAll(uint32_t device_id, uint8_t ep_address) {
  return ZX_ERR_NOT_SUPPORTED;
}

fpromise::promise<void, zx_status_t> UsbXhci::Timeout(uint16_t target_interrupter,
                                                      zx::time deadline) {
  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

fpromise::promise<void, zx_status_t> UsbXhci::DeviceOffline(uint32_t slot) {
  return fpromise::make_error_promise<zx_status_t>(ZX_ERR_NOT_SUPPORTED);
}

void UsbXhci::Shutdown(zx_status_t status) {
  running_ = false;
  GetTestHarness<EventRingHarness>()->RecordShutdown();
}

// TransferRing Methods
zx_status_t TransferRing::Init(size_t page_size, const zx::bti& bti, EventRing* ring, bool is_32bit,
                               fdf::MmioBuffer* mmio, UsbXhci* hci) {
  fbl::AutoLock _(&mutex_);
  hci_ = hci;
  return ZX_OK;
}

zx_status_t TransferRing::HandleShortPacket(TRB* short_trb, size_t short_length, TRB** first_trb) {
  return hci_->GetTestHarness<EventRingHarness>()->HandleShortPacket(short_trb, short_length,
                                                                     first_trb);
}

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBsUntil(TRB* end) {
  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> empty;
  return empty;
}

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBs() {
  return hci_->GetTestHarness<EventRingHarness>()->TakePendingTRBs();
}

TRB* TransferRing::PhysToVirt(zx_paddr_t paddr) {
  return (paddr == kFakeTrb) ? reinterpret_cast<TRB*>(kFakeTrbVirt) : nullptr;
}

zx_status_t TransferRing::CompleteTRB(TRB* trb, std::unique_ptr<TRBContext>* context) {
  return hci_->GetTestHarness<EventRingHarness>()->CompleteTRB(trb, context);
}

zx_status_t TransferRing::DeinitIfActive() { return ZX_OK; }

fpromise::promise<void, zx_status_t> EnumerateDevice(UsbXhci* hci, uint8_t port,
                                                     std::optional<HubInfo> hub_info) {
  auto harness = hci->GetTestHarness<EventRingHarness>();
  std::unique_ptr<Command> command = std::make_unique<Command>();
  fpromise::bridge<TRB*, zx_status_t> bridge;
  command->type = CommandType::EnumerateDevice;
  command->port = port;
  command->hub = hub_info;
  command->completer = std::move(bridge.completer);
  harness->AddCommand(std::move(command));
  return bridge.consumer.promise().discard_value();
}

TEST_F(EventRingHarness, ShortTransferTest) {
  InitSlot(1);
  TRB* start = trb();
  TRB trb;
  trb.ptr = kFakeTrb;
  Control::FromTRB(&trb).set_Type(Control::TransferEvent).ToTrb(&trb);
  TransferEvent* evt = static_cast<TransferEvent*>(&trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(2);
  evt->set_CompletionCode(CommandCompletionEvent::ShortPacket);
  evt->set_TransferLength(kShortTransferLength0);
  AddTRB(trb);
  evt->set_TransferLength(kShortTransferLength1);
  AddTRB(trb);
  evt->set_CompletionCode(CommandCompletionEvent::Success);
  evt->set_TransferLength(kFinalTransferLength);
  AddTRB(trb);
  size_t index = 0;
  TRB* trb_list[2];
  size_t shorts[2];
  SetShortPacketHandler([&](TRB* short_trb, size_t short_length, usb_xhci::TRB** first_trb) {
    *first_trb = nullptr;
    trb_list[index] = short_trb;
    shorts[index] = short_length;
    index++;
    return ZX_OK;
  });
  auto ctx = AllocateContext();
  size_t transfer_len;
  zx_status_t transfer_status;
  ctx->trb = start;
  std::optional<TestRequest> request;
  AllocateRequest(&request, 1, zx_system_get_page_size() * 3, 5, [&](TestRequest request) {
    transfer_status = request.request()->response.status;
    transfer_len = request.request()->response.actual;
  });
  ctx->short_transfer_len = kShortTransferLength;
  ctx->request = Borrow(std::move(*request));
  AddContext(std::move(ctx));
  SetCompletion(reinterpret_cast<TRB*>(kFakeTrbVirt));
  Interrupt();
  ASSERT_OK(transfer_status);
  ASSERT_EQ(transfer_len, kShortTransferLength);
  ASSERT_EQ(shorts[0], kShortTransferLength0);
  ASSERT_EQ(shorts[1], kShortTransferLength1);
  ASSERT_EQ(trb_list[0], reinterpret_cast<TRB*>(kFakeTrbVirt));
  ASSERT_EQ(trb_list[1], reinterpret_cast<TRB*>(kFakeTrbVirt));
}

TEST_F(EventRingHarness, NormalStall) {
  InitSlot(1);
  TRB* start = trb();
  TRB trb;
  trb.ptr = kFakeTrb;
  Control::FromTRB(&trb).set_Type(Control::TransferEvent).ToTrb(&trb);
  TransferEvent* evt = static_cast<TransferEvent*>(&trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(2);
  evt->set_CompletionCode(CommandCompletionEvent::StallError);
  evt->set_TransferLength(0);
  AddTRB(trb);
  auto ctx = AllocateContext();
  size_t transfer_len;
  zx_status_t transfer_status;
  ctx->trb = start;
  std::optional<TestRequest> request;
  AllocateRequest(&request, 1, zx_system_get_page_size() * 3, 5, [&](TestRequest request) {
    transfer_status = request.request()->response.status;
    transfer_len = request.request()->response.actual;
  });
  ctx->request = Borrow(std::move(*request));
  AddContext(std::move(ctx));

  auto trailing_ctx = AllocateContext();
  size_t trailing_len = 999;
  zx_status_t trailing_status = ZX_OK;
  trailing_ctx->trb = start;
  std::optional<TestRequest> trailing_request;
  ASSERT_OK(
      AllocateRequest(&trailing_request, 1, zx_system_get_page_size(), 5, [&](TestRequest req) {
        trailing_status = req.request()->response.status;
        trailing_len = req.request()->response.actual;
      }));
  ASSERT_TRUE(trailing_request.has_value());
  trailing_ctx->request = Borrow(std::move(*trailing_request));
  AddContext(std::move(trailing_ctx));

  SetCompletion(reinterpret_cast<TRB*>(kFakeTrbVirt));
  Interrupt();
  ASSERT_EQ(transfer_status, ZX_ERR_IO_REFUSED);
  ASSERT_EQ(transfer_len, 0UL);
  ASSERT_EQ(trailing_status, ZX_ERR_IO_REFUSED);
  ASSERT_EQ(trailing_len, 0UL);
}

TEST_F(EventRingHarness, StallWithNullTrbPointerAndEmptyRequestContexts) {
  InitSlot(1);
  TRB* start = trb();

  // 1. StallError with null TRB pointer (erdp_virt_->ptr == 0, xHCI 1.2 Section 4.17.4) drains
  // queued TRBContexts via TakePendingTRBs() and safely ignores empty-request contexts.
  TRB null_ptr_stall{};
  null_ptr_stall.ptr = 0;
  Control::FromTRB(&null_ptr_stall).set_Type(Control::TransferEvent).ToTrb(&null_ptr_stall);
  auto* null_evt = static_cast<TransferEvent*>(&null_ptr_stall);
  null_evt->set_SlotID(1);
  null_evt->set_EndpointID(2);
  null_evt->set_CompletionCode(CommandCompletionEvent::StallError);
  null_evt->set_TransferLength(0);
  AddTRB(null_ptr_stall);

  auto empty_pending_ctx = AllocateContext();
  empty_pending_ctx->trb = start;
  AddContext(std::move(empty_pending_ctx));

  zx_status_t drained_status = ZX_OK;
  size_t drained_len = 999;
  std::optional<TestRequest> drained_req;
  ASSERT_OK(AllocateRequest(&drained_req, 1, zx_system_get_page_size(), 5, [&](TestRequest req) {
    drained_status = req.request()->response.status;
    drained_len = req.request()->response.actual;
  }));
  ASSERT_TRUE(drained_req.has_value());
  auto req_ctx = AllocateContext();
  req_ctx->trb = start;
  req_ctx->request = Borrow(std::move(*drained_req));
  AddContext(std::move(req_ctx));

  Interrupt();
  EXPECT_EQ(drained_status, ZX_ERR_IO_REFUSED);
  EXPECT_EQ(drained_len, 0u);
  EXPECT_FALSE(HasCommand());

  // 2. StallError with a non-null TRB pointer and an empty-request head TRBContext does not crash.
  TRB head_stall{};
  head_stall.ptr = kFakeTrb;
  Control::FromTRB(&head_stall).set_Type(Control::TransferEvent).ToTrb(&head_stall);
  auto* head_evt = static_cast<TransferEvent*>(&head_stall);
  head_evt->set_SlotID(1);
  head_evt->set_EndpointID(2);
  head_evt->set_CompletionCode(CommandCompletionEvent::StallError);
  head_evt->set_TransferLength(0);
  AddTRB(head_stall);

  auto empty_head_ctx = AllocateContext();
  empty_head_ctx->trb = start;
  AddContext(std::move(empty_head_ctx));
  SetCompletion(reinterpret_cast<TRB*>(kFakeTrbVirt));

  Interrupt();
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, BadHubStallOnDtDeviceQualifier) {
  InitSlot(1);
  TRB* start = trb();
  TRB trb;
  trb.ptr = kFakeTrb;
  Control::FromTRB(&trb).set_Type(Control::TransferEvent).ToTrb(&trb);
  TransferEvent* evt = static_cast<TransferEvent*>(&trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(2);
  evt->set_CompletionCode(CommandCompletionEvent::StallError);
  evt->set_TransferLength(0);
  AddTRB(trb);
  auto ctx = AllocateContext();
  size_t transfer_len;
  zx_status_t transfer_status;
  ctx->trb = start;
  std::optional<TestRequest> request;
  usb_device_qualifier_descriptor_t descriptor;
  AllocateRequest(&request, 1, sizeof(usb_device_qualifier_descriptor_t), 0,
                  [&](TestRequest request) {
                    transfer_status = request.request()->response.status;
                    transfer_len = request.request()->response.actual;
                    usb_device_qualifier_descriptor_t* result;
                    request.Mmap(reinterpret_cast<void**>(&result));
                    memcpy(&descriptor, result, sizeof(usb_device_qualifier_descriptor_t));
                  });
  request->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  request->request()->setup.w_index = 0;
  request->request()->setup.w_value =
      usb_descriptor_w_value(fdescriptor::DescriptorType::kDeviceQualifier);
  request->request()->header.length = sizeof(usb_device_qualifier_descriptor_t);

  ctx->request = Borrow(std::move(*request));
  AddContext(std::move(ctx));
  SetCompletion(reinterpret_cast<TRB*>(kFakeTrbVirt));
  Interrupt();
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->type, CommandType::ResetEndpoint);
  ASSERT_EQ(cmd->endpoint, 0UL);
  ASSERT_EQ(cmd->device_id, 1UL);
  cmd->completer.complete_ok(nullptr);
  Interrupt();

  ASSERT_EQ(transfer_status, ZX_OK);
  ASSERT_EQ(transfer_len, sizeof(usb_device_qualifier_descriptor_t));
  ASSERT_EQ(descriptor.b_device_protocol, 0);
}

TEST_F(EventRingHarness, BadHubStallOnDtDeviceQualifierShortLength) {
  InitSlot(1);
  TRB* start = trb();
  TRB trb;
  trb.ptr = kFakeTrb;
  Control::FromTRB(&trb).set_Type(Control::TransferEvent).ToTrb(&trb);
  TransferEvent* evt = static_cast<TransferEvent*>(&trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(2);
  evt->set_CompletionCode(CommandCompletionEvent::StallError);
  evt->set_TransferLength(0);
  AddTRB(trb);
  auto ctx = AllocateContext();
  size_t transfer_len;
  zx_status_t transfer_status;
  ctx->trb = start;
  std::optional<TestRequest> request;
  AllocateRequest(&request, 1, sizeof(usb_device_qualifier_descriptor_t), 0,
                  [&](TestRequest request) {
                    transfer_status = request.request()->response.status;
                    transfer_len = request.request()->response.actual;
                  });
  request->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  request->request()->setup.w_index = 0;
  request->request()->setup.w_value =
      usb_descriptor_w_value(fdescriptor::DescriptorType::kDeviceQualifier);
  request->request()->header.length =
      8;  // Short length (8 < sizeof(usb_device_qualifier_descriptor_t))

  ctx->request = Borrow(std::move(*request));
  AddContext(std::move(ctx));
  SetCompletion(reinterpret_cast<TRB*>(kFakeTrbVirt));
  Interrupt();

  ASSERT_EQ(transfer_status, ZX_ERR_IO_REFUSED);
  ASSERT_EQ(transfer_len, 0UL);
}

TEST_F(EventRingHarness, USB2DeviceAttach) {
  ConnectDevice();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());
  LinkUp();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

TEST_F(EventRingHarness, USB2DeviceAttachBadStateTransition) {
  ConnectDevice();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());
  LinkUp();
  MakeQuantumPort();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_FALSE(HasCommand());
  LinkUp();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

TEST_F(EventRingHarness, Usb3DeviceAttach) {
  ConnectDevice();
  LinkUp();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

namespace {

TEST_F(EventRingHarness, Usb2DeviceAttachNonPollingPortSpeed) {
  ConnectDevice();
  // Simulate controller where PLS does not transition through Polling (e.g. RxDetect),
  // but PortSpeed indicates USB 2.0 (speed 3 = High Speed).
  port_sc().set_PLS(PORTSC::RxDetect);
  port_sc().set_PortSpeed(3);
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());
  LinkUp();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

TEST_F(EventRingHarness, HandleIRQClearsEhbWhenNoTrbsProcessed) {
  // Trigger HandleIRQ without queuing any event TRBs.
  // When processed_trbs == 0, ERDP is written with EHB=1 to clear Event Handler Busy
  // and acknowledge to the controller that the event ring was checked.
  Interrupt();
  EXPECT_TRUE(last_erdp().EHB());
}

TEST_F(EventRingHarness, Ring0BringupFailsWhenNotRunning) {
  driver_test().driver()->SetRunningForTesting(false);
  driver_test().driver()->SignalBringupForTesting();
  EXPECT_STATUS(Ring0Bringup(), ZX_ERR_BAD_STATE);
}

TEST_F(EventRingHarness, DestroyWithPendingPortStatusChangeWait) {
  ConnectDevice();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, ClearPendingPortStatusChangesAbortsWait) {
  ConnectDevice();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());
  event_ring()->ClearPendingPortStatusChanges();
  EXPECT_FALSE(driver_test().driver()->GetPortState(kPortNo).wait_for_port_status_change_);
}

TEST_F(EventRingHarness, PortStatusChangeEventInvalidPortIdIgnored) {
  ConnectDevice();
  // PortID 0 (underflow) and PortID > GetPortCount() (32 in test harness) should be ignored
  // without crashing or scheduling enumeration.
  InsertPortStatusChangeEvent(0);
  InsertPortStatusChangeEvent(33);
  InsertPortStatusChangeEvent(255);
  InsertPortStatusChangeEvent(kPortNo);
  Interrupt();
  EXPECT_TRUE(PortResetPending());
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, TransferEventInvalidSlotIdIgnored) {
  InitSlot(1);
  // SlotID 0 (underflow) and SlotID > max_slots_ (32 in test harness) should be ignored
  // without out-of-bounds access or crashing.
  for (uint8_t invalid_slot_id : {uint8_t{0}, uint8_t{33}, uint8_t{200}, uint8_t{255}}) {
    TRB trb;
    trb.ptr = kFakeTrb;
    Control::FromTRB(&trb).set_Type(Control::TransferEvent).ToTrb(&trb);
    auto* evt = static_cast<TransferEvent*>(&trb);
    evt->set_SlotID(invalid_slot_id);
    evt->set_EndpointID(2);
    evt->set_CompletionCode(CommandCompletionEvent::Success);
    evt->set_TransferLength(kFinalTransferLength);
    AddTRB(trb);
  }
  Interrupt();
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, Ring0BringupUsb2PortResetWithEarlyCscEvent) {
  port_sc()
      .set_CCS(true)
      .set_PED(false)
      .set_PR(false)
      .set_PLS(PORTSC::Polling)
      .set_PortSpeed(1)
      .set_CSC(true);
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  ASSERT_TRUE(PortResetPending());
  EXPECT_FALSE(port_sc().CSC());
  EXPECT_EQ(port_sc_write_count(), 1UL);
  ASSERT_FALSE(HasCommand());
  EXPECT_TRUE(driver_test().driver()->GetPortState(kPortNo).wait_for_port_status_change_);

  // An already-queued CSC=1 PortStatusChangeEvent TRB arrives while the USB 2.0 port reset is
  // still in progress (PED=0, PR=1). Even if the controller transiently reports PLS=U0 while
  // PR=1, it must not enumerate prematurely or drop the port status change wait.
  port_sc().set_PLS(PORTSC::U0);
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_FALSE(HasCommand());
  EXPECT_TRUE(driver_test().driver()->GetPortState(kPortNo).wait_for_port_status_change_);

  // Complete the USB 2.0 port reset (PR=0, PED=1, PLS=U0, PRC=1).
  port_sc().set_PR(false).set_PED(true).set_PLS(PORTSC::U0).set_PRC(true);
  InsertPortStatusChangeEvent();
  Interrupt();
  EXPECT_FALSE(port_sc().PRC());
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

TEST_F(EventRingHarness, Ring0BringupUsb2PortResetCompletesViaPollingWithoutEventTrb) {
  port_sc()
      .set_CCS(true)
      .set_PED(false)
      .set_PR(false)
      .set_PLS(PORTSC::Polling)
      .set_PortSpeed(3)
      .set_CSC(true);
  SetPortScWriteHook([this](const PORTSC& written) {
    if (written.PR()) {
      port_sc().set_PR(false).set_PED(true).set_PLS(PORTSC::U0).set_PRC(true);
    }
  });
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  EXPECT_FALSE(port_sc().CSC());
  EXPECT_FALSE(port_sc().PRC());
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

TEST_F(EventRingHarness, Ring0BringupUsb2PortResetCompletesAfterMultiplePolls) {
  port_sc()
      .set_CCS(true)
      .set_PED(false)
      .set_PR(false)
      .set_PLS(PORTSC::Polling)
      .set_PortSpeed(3)
      .set_CSC(true);
  // The port reset only completes after several PORTSC polls, and the controller never emits a
  // Port Status Change Event TRB for it.
  constexpr size_t kReadsBeforeResetCompletes = 10;
  SetPortScReadHook([this]() {
    if (port_sc_read_count() == kReadsBeforeResetCompletes) {
      port_sc().set_PR(false).set_PED(true).set_PLS(PORTSC::U0).set_PRC(true);
    }
  });
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  EXPECT_GE(port_sc_read_count(), kReadsBeforeResetCompletes);
  EXPECT_FALSE(port_sc().PRC());
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(cmd->hub);
}

TEST_F(EventRingHarness, Ring0BringupUsb2PortStatusChangePollIsBounded) {
  port_sc()
      .set_CCS(true)
      .set_PED(false)
      .set_PR(false)
      .set_PLS(PORTSC::Polling)
      .set_PortSpeed(3)
      .set_CSC(true);
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  // The port reset never completes: polling stops after a bounded number of attempts (the fake
  // Timeout() resolves immediately) while the wait stays armed for a Port Status Change Event TRB.
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());
  EXPECT_TRUE(driver_test().driver()->GetPortState(kPortNo).wait_for_port_status_change_);
  EXPECT_LT(port_sc_read_count(), 200UL);

  port_sc().set_PR(false).set_PED(true).set_PLS(PORTSC::U0).set_PRC(true);
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
}

TEST_F(EventRingHarness, Ring0BringupUsb2FailedPortResetDoesNotKeepWaiting) {
  port_sc()
      .set_CCS(true)
      .set_PED(false)
      .set_PR(false)
      .set_PLS(PORTSC::Polling)
      .set_PortSpeed(3)
      .set_CSC(true);
  // The port reset completes (PR=0, PRC=1) but the port stays disabled (PED=0).
  SetPortScWriteHook([this](const PORTSC& written) {
    if (written.PR()) {
      port_sc().set_PR(false).set_PED(false).set_PRC(true);
    }
  });
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  EXPECT_FALSE(port_sc().PRC());
  EXPECT_FALSE(HasCommand());
  // No wait may stay armed for a reset that is no longer in progress, otherwise the enumeration
  // queue would be blocked for every port.
  EXPECT_FALSE(driver_test().driver()->GetPortState(kPortNo).wait_for_port_status_change_);

  // A later port status change on another port is still processed.
  second_port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  InsertPortStatusChangeEvent(kSecondPortNo);
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kSecondPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
}

TEST_F(EventRingHarness, Ring0BringupContinuesScanAfterIoNotPresent) {
  // Two SuperSpeed devices already have their links up when Ring0Bringup() runs.
  port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  second_port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(HasCommand());

  // The first device is unplugged during enumeration. The scan must still reach the second port.
  cmd->completer.complete_error(ZX_ERR_IO_NOT_PRESENT);
  event_ring()->RunUntilIdle();
  ASSERT_TRUE(HasCommand());
  cmd = GetCommand();
  ASSERT_EQ(cmd->port, kSecondPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
}

TEST_F(EventRingHarness, Ring0BringupStopsScanWhenControllerNotRunning) {
  // Two SuperSpeed devices already have their links up when Ring0Bringup() runs.
  port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  second_port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(HasCommand());

  // Controller teardown starts while the first port's enumeration is in flight and aborts the
  // pending command with ZX_ERR_IO_NOT_PRESENT. The scan must stop instead of starting enumeration
  // on later ports.
  driver_test().driver()->SetRunningForTesting(false);
  cmd->completer.complete_error(ZX_ERR_IO_NOT_PRESENT);
  event_ring()->RunUntilIdle();
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, Ring0BringupStopsScanAfterSuccessWhenControllerNotRunning) {
  // Two SuperSpeed devices already have their links up when Ring0Bringup() runs.
  port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  second_port_sc().set_CCS(true).set_PED(true).set_PR(false).set_PLS(PORTSC::U0).set_PortSpeed(4);
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->port, kPortNo);
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  ASSERT_FALSE(HasCommand());

  // Even if the in-flight port operation completes with ZX_OK just as the controller stops, the
  // scan must halt instead of advancing to later ports.
  driver_test().driver()->SetRunningForTesting(false);
  cmd->completer.complete_ok(nullptr);
  event_ring()->RunUntilIdle();
  EXPECT_FALSE(HasCommand());
}

class StallEventHandler : public fidl::SyncEventHandler<fuchsia_hardware_usb_endpoint::Endpoint> {
 public:
  void OnCompletion(
      fidl::Event<fuchsia_hardware_usb_endpoint::Endpoint::OnCompletion>& event) override {
    completions = std::move(event.completion());
  }

  std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
};

TEST_F(EventRingHarness, FidlRequestStallCompletesWithIoRefused) {
  InitSlot(1);
  auto state = driver_test().driver()->GetDeviceState(0);
  ASSERT_TRUE(state);

  auto endpoints = fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  TRB* expected_trb = nullptr;
  {
    fbl::AutoLock _(&state->transaction_lock());
    state->GetEndpoint(0).Connect(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                  std::move(endpoints.server), state);
    expected_trb = state->GetEndpoint(0).transfer_ring().PhysToVirt(kFakeTrb);
  }

  TRB* start = trb();
  TRB stall_trb{};
  stall_trb.ptr = kFakeTrb;
  Control::FromTRB(&stall_trb).set_Type(Control::TransferEvent).ToTrb(&stall_trb);
  auto* evt = static_cast<TransferEvent*>(&stall_trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(2);
  evt->set_CompletionCode(CommandCompletionEvent::StallError);
  evt->set_TransferLength(0);
  AddTRB(stall_trb);

  usb::FidlRequest fidl_req(usb::EndpointType::BULK);
  fidl_req->defer_completion() = false;

  auto ctx = AllocateContext();
  ctx->trb = start;
  ctx->request.emplace(std::move(fidl_req));
  AddContext(std::move(ctx));
  SetCompletion(expected_trb);

  Interrupt();

  StallEventHandler handler;
  ASSERT_TRUE(handler.HandleOneEvent(endpoints.client).ok());
  ASSERT_EQ(handler.completions.size(), 1u);
  EXPECT_EQ(handler.completions[0].status(), ZX_ERR_IO_REFUSED);
  EXPECT_EQ(handler.completions[0].transfer_size(), 0u);
  EXPECT_FALSE(HasCommand());
  {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_TRUE(state->GetEndpoint(0).transfer_ring().stalled());
  }

  endpoints.client.reset();
  driver_test().runtime().RunUntilIdle();
}

TEST_F(EventRingHarness, FidlControlRequestStallCompletesWithIoRefused) {
  InitSlot(1);
  auto state = driver_test().driver()->GetDeviceState(0);
  ASSERT_TRUE(state);

  auto endpoints = fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
  TRB* expected_trb = nullptr;
  {
    fbl::AutoLock _(&state->transaction_lock());
    ASSERT_OK(state->GetEndpoint().Init(nullptr, nullptr));
    state->GetEndpoint().Connect(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                 std::move(endpoints.server), state);
    expected_trb = state->GetEndpoint().transfer_ring().PhysToVirt(kFakeTrb);
  }

  TRB* start = trb();
  TRB stall_trb{};
  stall_trb.ptr = kFakeTrb;
  Control::FromTRB(&stall_trb).set_Type(Control::TransferEvent).ToTrb(&stall_trb);
  auto* evt = static_cast<TransferEvent*>(&stall_trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(1);
  evt->set_CompletionCode(CommandCompletionEvent::StallError);
  evt->set_TransferLength(0);
  AddTRB(stall_trb);

  usb::FidlRequest fidl_req(usb::EndpointType::CONTROL);
  fidl_req->defer_completion() = false;
  fidl_req->information()->control()->setup(
      fdescriptor::UsbSetup()
          .bm_request_type(kStandardDeviceIn)
          .b_request(fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor))
          .w_value(usb_descriptor_w_value(fdescriptor::DescriptorType::kDeviceQualifier))
          .w_index(0)
          .w_length(sizeof(usb_device_qualifier_descriptor_t)));

  auto ctx = AllocateContext();
  ctx->trb = start;
  ctx->request.emplace(std::move(fidl_req));
  AddContext(std::move(ctx));
  SetCompletion(expected_trb);

  Interrupt();

  StallEventHandler handler;
  ASSERT_TRUE(handler.HandleOneEvent(endpoints.client).ok());
  ASSERT_EQ(handler.completions.size(), 1u);
  EXPECT_EQ(handler.completions[0].status(), ZX_ERR_IO_REFUSED);
  EXPECT_EQ(handler.completions[0].transfer_size(), 0u);
  EXPECT_FALSE(HasCommand());
  {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_TRUE(state->GetEndpoint().transfer_ring().stalled());
  }

  endpoints.client.reset();
  driver_test().runtime().RunUntilIdle();
}

TEST_F(EventRingHarness, MissedServiceErrorOnAlreadyDrainedRingDoesNotCrash) {
  InitSlot(1);
  ASSERT_FALSE(driver_test().driver()->IsHaltedOrStopping());

  // Ring has no pending contexts (already drained by AbortPendingCommands / DrainPendingOnce).
  TRB missed_trb{};
  Control::FromTRB(&missed_trb).set_Type(Control::TransferEvent).ToTrb(&missed_trb);
  auto* evt = static_cast<TransferEvent*>(&missed_trb);
  evt->set_SlotID(1);
  evt->set_EndpointID(2);
  evt->set_CompletionCode(CommandCompletionEvent::MissedServiceError);
  AddTRB(missed_trb);

  Interrupt();
  EXPECT_FALSE(shutdown_called());
}

TEST_F(EventRingHarness, ShortPacketOnAlreadyDrainedRingWhenHaltedOrDisconnectingDoesNotCrash) {
  InitSlot(1);
  InitSlot(2);
  auto state1 = driver_test().driver()->GetDeviceState(0);
  ASSERT_TRUE(state1);
  {
    fbl::AutoLock _(&state1->transaction_lock());
    state1->Disconnect();
  }

  // Case 1: Controller is running (!IsHaltedOrStopping()), slot 1 is disconnecting, ring is empty.
  ASSERT_FALSE(driver_test().driver()->IsHaltedOrStopping());
  TRB short_trb{};
  short_trb.ptr = kFakeTrb;
  Control::FromTRB(&short_trb).set_Type(Control::TransferEvent).ToTrb(&short_trb);
  auto* short_evt = static_cast<TransferEvent*>(&short_trb);
  short_evt->set_SlotID(1);
  short_evt->set_EndpointID(2);
  short_evt->set_CompletionCode(CommandCompletionEvent::ShortPacket);
  short_evt->set_TransferLength(4);
  AddTRB(short_trb);

  TRB success_trb{};
  success_trb.ptr = kFakeTrb;
  Control::FromTRB(&success_trb).set_Type(Control::TransferEvent).ToTrb(&success_trb);
  auto* success_evt = static_cast<TransferEvent*>(&success_trb);
  success_evt->set_SlotID(1);
  success_evt->set_EndpointID(2);
  success_evt->set_CompletionCode(CommandCompletionEvent::Success);
  AddTRB(success_trb);

  Interrupt();
  EXPECT_FALSE(shutdown_called());

  // Case 2: Controller halted (USBSTS.HCH = 1) while running_ == true and slot 2 is NOT
  // disconnecting (modeling concurrent AbortPendingCommands() while an interrupter drains
  // already-queued Transfer Events and Command Completion Events).
  SetControllerHalted(true);
  ASSERT_TRUE(driver_test().driver()->Running());
  ASSERT_TRUE(driver_test().driver()->IsHaltedOrStopping());

  short_evt->set_SlotID(2);
  AddTRB(short_trb);

  TRB cmd_trb{};
  cmd_trb.ptr = kFakeTrb;
  Control::FromTRB(&cmd_trb).set_Type(Control::CommandCompletionEvent).ToTrb(&cmd_trb);
  auto* cmd_evt = static_cast<CommandCompletionEvent*>(&cmd_trb);
  cmd_evt->set_CompletionCode(CommandCompletionEvent::Success);
  AddTRB(cmd_trb);

  Interrupt();
  EXPECT_FALSE(shutdown_called());
}

TEST_F(EventRingHarness, ScheduleTaskIgnoresBadStateWhenNotRunning) {
  driver_test().driver()->SetRunningForTesting(false);
  event_ring()->ScheduleTask(fpromise::make_error_promise<zx_status_t>(ZX_ERR_BAD_STATE));
  event_ring()->RunUntilIdle();
  EXPECT_FALSE(shutdown_called());

  driver_test().driver()->SetRunningForTesting(true);
  event_ring()->ScheduleTask(fpromise::make_error_promise<zx_status_t>(ZX_ERR_BAD_STATE));
  event_ring()->RunUntilIdle();
  EXPECT_TRUE(shutdown_called());
}

TEST_F(EventRingHarness, Ring0BringupClearsPortLinkChangeWithoutCsc) {
  port_sc().set_CCS(false).set_CSC(false).set_PP(true).set_PLS(PORTSC::RxDetect).set_PLC(true);
  std::optional<PORTSC> written_sc;
  SetPortScWriteHook([&](const PORTSC& written) { written_sc = written; });

  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());

  ASSERT_TRUE(written_sc.has_value());
  EXPECT_EQ(port_sc_write_count(), 1UL);
  EXPECT_TRUE(written_sc->PLC());
  EXPECT_FALSE(written_sc->CEC());
  EXPECT_FALSE(written_sc->CSC());
  EXPECT_TRUE(written_sc->PP());
  EXPECT_FALSE(written_sc->PED());
  EXPECT_FALSE(written_sc->LWS());
  EXPECT_FALSE(written_sc->PR());
  EXPECT_FALSE(port_sc().PLC());
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, Ring0BringupClearsPortConfigErrorChangeWithoutCsc) {
  port_sc().set_CCS(false).set_CSC(false).set_PP(true).set_PLS(PORTSC::RxDetect).set_CEC(true);
  std::optional<PORTSC> written_sc;
  SetPortScWriteHook([&](const PORTSC& written) { written_sc = written; });

  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());

  ASSERT_TRUE(written_sc.has_value());
  EXPECT_EQ(port_sc_write_count(), 1UL);
  EXPECT_TRUE(written_sc->CEC());
  EXPECT_FALSE(written_sc->PLC());
  EXPECT_FALSE(written_sc->CSC());
  EXPECT_TRUE(written_sc->PP());
  EXPECT_FALSE(written_sc->PED());
  EXPECT_FALSE(written_sc->LWS());
  EXPECT_FALSE(written_sc->PR());
  EXPECT_FALSE(port_sc().CEC());
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, HandlePortStatusChangeEventClearsPlcAndCecWithoutCsc) {
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());
  ASSERT_EQ(port_sc_write_count(), 0UL);

  port_sc()
      .set_CCS(false)
      .set_CSC(false)
      .set_PP(true)
      .set_PLS(PORTSC::RxDetect)
      .set_PLC(true)
      .set_CEC(true);
  InsertPortStatusChangeEvent();
  Interrupt();

  EXPECT_EQ(port_sc_write_count(), 1UL);
  EXPECT_FALSE(port_sc().PLC());
  EXPECT_FALSE(port_sc().CEC());
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, HandlePortStatusChangeEventClearsPlcAndCecOnEnabledPortWithoutDisabling) {
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());

  // Bring the port to an already-enumerated, connected, enabled U0 state.
  ConnectDevice();
  port_sc().set_PP(true);
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(PortResetPending());
  ASSERT_FALSE(HasCommand());

  LinkUp();
  InsertPortStatusChangeEvent();
  Interrupt();
  ASSERT_TRUE(HasCommand());
  auto cmd = GetCommand();
  ASSERT_EQ(cmd->type, CommandType::EnumerateDevice);
  cmd->completer.complete_ok(nullptr);
  event_ring()->RunUntilIdle();
  ASSERT_FALSE(HasCommand());
  const size_t writes_before = port_sc_write_count();

  // Trigger PLC and CEC without CSC while CCS=1, PED=1, PLS=U0.
  port_sc()
      .set_CCS(true)
      .set_PED(true)
      .set_PP(true)
      .set_PLS(PORTSC::U0)
      .set_CSC(false)
      .set_PLC(true)
      .set_CEC(true);
  std::optional<PORTSC> written_sc;
  SetPortScWriteHook([&](const PORTSC& written) { written_sc = written; });

  InsertPortStatusChangeEvent();
  Interrupt();

  ASSERT_TRUE(written_sc.has_value());
  EXPECT_EQ(port_sc_write_count(), writes_before + 1UL);
  EXPECT_TRUE(written_sc->PLC());
  EXPECT_TRUE(written_sc->CEC());
  EXPECT_FALSE(written_sc->CSC());
  EXPECT_TRUE(written_sc->PP());
  // PED is RW1CS; writing 1 would disable the active port.
  EXPECT_FALSE(written_sc->PED());
  EXPECT_FALSE(written_sc->LWS());
  EXPECT_FALSE(written_sc->PR());
  EXPECT_FALSE(port_sc().PLC());
  EXPECT_FALSE(port_sc().CEC());
  EXPECT_TRUE(port_sc().PED());
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, Usb2AttachClearsCscPlcAndCecInSingleWrite) {
  port_sc()
      .set_CCS(true)
      .set_PED(false)
      .set_PR(false)
      .set_PP(true)
      .set_PLS(PORTSC::Polling)
      .set_PortSpeed(3)
      .set_CSC(true)
      .set_PLC(true)
      .set_CEC(true);
  driver_test().driver()->SignalBringupForTesting();
  ASSERT_OK(Ring0Bringup());

  EXPECT_TRUE(PortResetPending());
  EXPECT_EQ(port_sc_write_count(), 1UL);
  EXPECT_FALSE(port_sc().CSC());
  EXPECT_FALSE(port_sc().PLC());
  EXPECT_FALSE(port_sc().CEC());
  EXPECT_FALSE(HasCommand());
}

TEST_F(EventRingHarness, InterrupterTimeoutAndConcurrentStop) {
  Interrupter& interrupter = driver_test().driver()->interrupter(1);
  const RuntimeRegisterOffset regoffset = RuntimeRegisterOffset::Get().ReadFrom(mmio_buffer());
  ASSERT_OK(interrupter.Init(1, zx_system_get_page_size(), mmio_buffer(), regoffset, kErstMax,
                             DoorbellOffset::Get().ReadFrom(mmio_buffer()), driver_test().driver(),
                             HCCPARAMS1::Get().ReadFrom(mmio_buffer()), dcbaa()));
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupter.GetIrq()));
  auto stop_interrupter = fit::defer([&interrupter] { interrupter.Stop(); });

  // Stop() before Start() is a safe no-op, and Timeout() before Start() rejects cleanly.
  interrupter.Stop();
  EXPECT_STATUS(RunImmediateTimeout(interrupter, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);

  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));
  // Starting an already active interrupter is idempotent.
  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));

  // An expired Timeout() while active resolves with fpromise::ok() and drives interrupter(1)'s
  // event ring via hci_->RunUntilIdle(1) without manual RunUntilIdle().
  auto fired = ScheduleTimeout(interrupter, &interrupter.ring(), zx::time::infinite_past());
  // Enqueue a follow-up immediate Timeout() after ScheduleTask() returns so hci_->RunUntilIdle(1)
  // is guaranteed to run after the continuation is queued even if the first callback fired early.
  interrupter.ring().ScheduleTask(interrupter.Timeout(zx::time::infinite_past()));
  fired->done.Wait();
  EXPECT_OK(fired->status.load(std::memory_order_acquire));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter));
  // With enable_suspend() == false, StartIrqThread() must not connect to ActivityGovernor.
  EXPECT_EQ(ActivityGovernorConnectCount(), 0u);
  EXPECT_EQ(ActivityGovernorRegisterCount(), 0u);

  auto pending = ScheduleTimeout(interrupter, event_ring(), zx::time::infinite());
  event_ring()->RunUntilIdle();
  EXPECT_FALSE(pending->done.signaled());

  // Block interrupter's dispatcher inside a ring task triggered by an immediate Timeout() so a
  // second Stop() thread deterministically enters Stop() while the first Stop() is waiting on
  // teardown, exercising the StoppingState waiter path.
  auto task_entered = std::make_shared<libsync::Completion>();
  auto release_task = std::make_shared<libsync::Completion>();
  libsync::Completion t1_done;
  libsync::Completion t2_done;
  std::thread t1;
  std::thread t2;
  auto cleanup = fit::defer([&] {
    release_task->Signal();
    if (t2.joinable()) {
      t2.join();
    }
    if (t1.joinable()) {
      t1.join();
    }
    interrupter.Stop();
  });

  interrupter.ring().ScheduleTask(
      fpromise::make_promise([task_entered, release_task] -> fpromise::result<void, zx_status_t> {
        task_entered->Signal();
        release_task->Wait();
        return fpromise::ok();
      }));
  interrupter.ring().ScheduleTask(interrupter.Timeout(zx::time::infinite_past()));
  task_entered->Wait();
  // Queue an already-due timeout while the dispatcher is blocked so it executes after Stop()
  // clears active_ and exercises the early-return teardown branch in Interrupter::Timeout().
  auto raced = ScheduleTimeout(interrupter, event_ring(), zx::time::infinite_past());

  t1 = std::thread([&interrupter, &t1_done] {
    interrupter.Stop();
    t1_done.Signal();
  });
  while (!InterrupterTestPeer::IsStopping(interrupter) && !t1_done.signaled()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  ASSERT_TRUE(InterrupterTestPeer::IsStopping(interrupter));
  ASSERT_FALSE(t1_done.signaled());
  ASSERT_FALSE(interrupter.active());

  t2 = std::thread([&interrupter, &t2_done] {
    interrupter.Stop();
    t2_done.Signal();
  });
  while (!InterrupterTestPeer::HasStoppingWaiter(interrupter) && !t2_done.signaled()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  ASSERT_TRUE(InterrupterTestPeer::HasStoppingWaiter(interrupter));
  EXPECT_FALSE(t1_done.signaled());
  EXPECT_FALSE(t2_done.signaled());
  // Timeout() called while paused in StoppingState must reject immediately.
  EXPECT_STATUS(RunImmediateTimeout(interrupter, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);

  release_task->Signal();
  t1_done.Wait();
  t2_done.Wait();
  t1.join();
  t2.join();

  // Shutting down the interrupter while a delayed timeout is pending cancels the task, and an
  // already-due timeout that runs after Stop() clears active_ returns early; both must resolve
  // with ZX_ERR_IO_NOT_PRESENT rather than abandoning continuations.
  event_ring()->RunUntilIdle();
  ASSERT_TRUE(pending->done.signaled());
  EXPECT_STATUS(pending->status.load(std::memory_order_acquire), ZX_ERR_IO_NOT_PRESENT);
  ASSERT_TRUE(raced->done.signaled());
  EXPECT_STATUS(raced->status.load(std::memory_order_acquire), ZX_ERR_IO_NOT_PRESENT);

  // Flush succeeds and Timeout() rejects cleanly after Stop().
  EXPECT_OK(InterrupterTestPeer::Flush(interrupter));
  EXPECT_STATUS(RunImmediateTimeout(interrupter, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);

  // Stopping immediately after Start() without first flushing StartIrqThread() must tear down
  // cleanly, reject Timeout(), and leave the interrupter restartable.
  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));
  interrupter.Stop();
  EXPECT_FALSE(interrupter.active());
  EXPECT_OK(InterrupterTestPeer::Flush(interrupter));
  EXPECT_STATUS(RunImmediateTimeout(interrupter, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);

  // Restarting after Stop() creates a fresh shutdown completion and stops cleanly again.
  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter));
  EXPECT_TRUE(interrupter.active());
  interrupter.Stop();
}

class EventRingSuspendHarness : public EventRingHarness {
 protected:
  bool enable_suspend() const override { return true; }
};

TEST_F(EventRingSuspendHarness, InterrupterStartFailureAndIrqBeginFailureResetActiveAndIman) {
  Interrupter& interrupter = driver_test().driver()->interrupter(1);
  Interrupter interrupter0;
  const RuntimeRegisterOffset regoffset = RuntimeRegisterOffset::Get().ReadFrom(mmio_buffer());
  std::atomic<uint32_t> iman0_val{0};
  std::atomic<uint32_t> iman1_val{0};
  const zx_off_t iman0_addr = IMAN::Get(regoffset, 0).addr();
  const zx_off_t iman1_addr = IMAN::Get(regoffset, 1).addr();
  mmio_region()[iman0_addr].SetReadCallback(
      [&iman0_val] { return iman0_val.load(std::memory_order_acquire); });
  mmio_region()[iman0_addr].SetWriteCallback([&iman0_val](uint64_t val) {
    iman0_val.store(static_cast<uint32_t>(val), std::memory_order_release);
  });
  mmio_region()[iman1_addr].SetReadCallback(
      [&iman1_val] { return iman1_val.load(std::memory_order_acquire); });
  mmio_region()[iman1_addr].SetWriteCallback([&iman1_val](uint64_t val) {
    iman1_val.store(static_cast<uint32_t>(val), std::memory_order_release);
  });
  auto clear_callbacks = fit::defer([this, &interrupter, &interrupter0, iman0_addr, iman1_addr] {
    interrupter0.Stop();
    interrupter.Stop();
    driver_test().driver()->SetRunningForTesting(true);
    mmio_region()[iman0_addr].SetReadCallback([] { return 0u; });
    mmio_region()[iman0_addr].SetWriteCallback([](uint64_t) {});
    mmio_region()[iman1_addr].SetReadCallback([] { return 0u; });
    mmio_region()[iman1_addr].SetWriteCallback([](uint64_t) {});
  });

  // Start() before Init() allocates an event ring segment fails cleanly with IMAN.IE disabled and
  // without connecting to ActivityGovernor.
  EXPECT_STATUS(interrupter.Start(regoffset, mmio_buffer()->View(0)), ZX_ERR_BAD_STATE);
  EXPECT_FALSE(interrupter.active());
  EXPECT_EQ(IMAN::Get(regoffset, 1).FromValue(iman1_val.load(std::memory_order_acquire)).IE(), 0u);
  EXPECT_EQ(ActivityGovernorConnectCount(), 0u);

  ASSERT_OK(interrupter.Init(1, zx_system_get_page_size(), mmio_buffer(), regoffset, kErstMax,
                             DoorbellOffset::Get().ReadFrom(mmio_buffer()), driver_test().driver(),
                             HCCPARAMS1::Get().ReadFrom(mmio_buffer()), dcbaa()));

  // Leave the interrupter without a valid IRQ handle so StartIrqThread() fails asynchronously
  // with ZX_ERR_BAD_HANDLE and Start()'s error path clears IMAN.IE and active_ without connecting
  // to ActivityGovernor.
  interrupter.GetIrq().reset();

  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter));
  EXPECT_FALSE(interrupter.active());
  EXPECT_EQ(IMAN::Get(regoffset, 1).FromValue(iman1_val.load(std::memory_order_acquire)).IE(), 0u);
  EXPECT_EQ(ActivityGovernorConnectCount(), 0u);
  EXPECT_EQ(ActivityGovernorRegisterCount(), 0u);
  // Subsequent Start() and Timeout() must reject while the failed dispatcher is still in
  // RunningState with active_ false.
  EXPECT_STATUS(interrupter.Start(regoffset, mmio_buffer()->View(0)), ZX_ERR_BAD_STATE);
  EXPECT_STATUS(RunImmediateTimeout(interrupter, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);

  interrupter.Stop();

  // Exercise Ring0Bringup() failure rollback on interrupter 0: irq_handler_.Begin() succeeds,
  // Ring0Bringup() fails because running_ is false, and Start()'s error path must cancel
  // irq_handler_, clear IMAN.IE and active_, and avoid connecting to ActivityGovernor.
  ASSERT_OK(interrupter0.Init(0, zx_system_get_page_size(), mmio_buffer(), regoffset, kErstMax,
                              DoorbellOffset::Get().ReadFrom(mmio_buffer()), driver_test().driver(),
                              HCCPARAMS1::Get().ReadFrom(mmio_buffer()), dcbaa()));
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupter0.GetIrq()));
  driver_test().driver()->SignalBringupForTesting();
  driver_test().driver()->SetRunningForTesting(false);
  ASSERT_OK(interrupter0.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter0));
  EXPECT_FALSE(interrupter0.active());
  EXPECT_EQ(IMAN::Get(regoffset, 0).FromValue(iman0_val.load(std::memory_order_acquire)).IE(), 0u);
  EXPECT_EQ(ActivityGovernorConnectCount(), 0u);
  EXPECT_EQ(ActivityGovernorRegisterCount(), 0u);
  EXPECT_STATUS(RunImmediateTimeout(interrupter0, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);
  // Verify deterministically on the synchronized dispatcher that irq_handler_ was already canceled
  // before returning, so triggering the virtual IRQ cannot fire into an unemplaced wake_lease_.
  EXPECT_STATUS(InterrupterTestPeer::CancelIrqHandler(interrupter0), ZX_ERR_NOT_FOUND);
  ASSERT_OK(interrupter0.GetIrq().trigger(0, zx::clock::get_boot()));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter0));
  interrupter0.Stop();
  driver_test().driver()->SetRunningForTesting(true);

  // After Stop() tears down the failed dispatcher, Start() succeeds with a valid interrupt,
  // connects to ActivityGovernor, registers a live SuspendBlocker, and tears it down on Stop().
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupter.GetIrq()));
  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter));
  EXPECT_TRUE(interrupter.active());
  EXPECT_EQ(IMAN::Get(regoffset, 1).FromValue(iman1_val.load(std::memory_order_acquire)).IE(), 1u);
  EXPECT_EQ(ActivityGovernorConnectCount(), 1u);
  EXPECT_EQ(ActivityGovernorRegisterCount(), 1u);
  auto blockers = TakeSuspendBlockers();
  ASSERT_EQ(blockers.size(), 1u);

  interrupter.Stop();
  zx_signals_t observed = 0;
  EXPECT_OK(
      blockers[0].channel().wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite_past(), &observed));
  EXPECT_TRUE((observed & ZX_CHANNEL_PEER_CLOSED) != 0);
}

TEST_F(EventRingSuspendHarness, InterrupterMultiInterrupterAndRestartConnectActivityGovernor) {
  Interrupter& interrupter1 = driver_test().driver()->interrupter(1);
  Interrupter interrupter2;
  const RuntimeRegisterOffset regoffset = RuntimeRegisterOffset::Get().ReadFrom(mmio_buffer());
  auto stop_all = fit::defer([&interrupter1, &interrupter2] {
    interrupter2.Stop();
    interrupter1.Stop();
  });

  ASSERT_OK(interrupter1.Init(1, zx_system_get_page_size(), mmio_buffer(), regoffset, kErstMax,
                              DoorbellOffset::Get().ReadFrom(mmio_buffer()), driver_test().driver(),
                              HCCPARAMS1::Get().ReadFrom(mmio_buffer()), dcbaa()));
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupter1.GetIrq()));
  ASSERT_OK(interrupter2.Init(2, zx_system_get_page_size(), mmio_buffer(), regoffset, kErstMax,
                              DoorbellOffset::Get().ReadFrom(mmio_buffer()), driver_test().driver(),
                              HCCPARAMS1::Get().ReadFrom(mmio_buffer()), dcbaa()));
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupter2.GetIrq()));

  ASSERT_OK(interrupter1.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(interrupter2.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter1));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter2));
  EXPECT_EQ(ActivityGovernorConnectCount(), 2u);
  EXPECT_EQ(ActivityGovernorRegisterCount(), 2u);

  // Stopping and restarting an interrupter connects a fresh ActivityGovernor client.
  interrupter1.Stop();
  ASSERT_OK(interrupter1.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter1));
  EXPECT_EQ(ActivityGovernorConnectCount(), 3u);
  EXPECT_EQ(ActivityGovernorRegisterCount(), 3u);
  auto blockers = TakeSuspendBlockers();
  ASSERT_EQ(blockers.size(), 3u);

  interrupter2.Stop();
  interrupter1.Stop();
  for (const auto& blocker : blockers) {
    zx_signals_t observed = 0;
    EXPECT_OK(
        blocker.channel().wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite_past(), &observed));
    EXPECT_TRUE((observed & ZX_CHANNEL_PEER_CLOSED) != 0);
  }
}

TEST_F(EventRingHarness, InterrupterConcurrentStartAndStopDuringStartingState) {
  Interrupter& interrupter = driver_test().driver()->interrupter(1);
  const RuntimeRegisterOffset regoffset = RuntimeRegisterOffset::Get().ReadFrom(mmio_buffer());
  ASSERT_OK(interrupter.Init(1, zx_system_get_page_size(), mmio_buffer(), regoffset, kErstMax,
                             DoorbellOffset::Get().ReadFrom(mmio_buffer()), driver_test().driver(),
                             HCCPARAMS1::Get().ReadFrom(mmio_buffer()), dcbaa()));
  ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupter.GetIrq()));

  // Run Start() on a driver-runtime background dispatcher rather than a plain std::thread so
  // fdf::SynchronizedDispatcher::Create() sees a valid driver-runtime thread context.
  fdf::UnownedSynchronizedDispatcher start_dispatcher =
      driver_test().runtime().StartBackgroundDispatcher();

  std::atomic<uint32_t> iman_val{0};
  const zx_off_t iman_addr = IMAN::Get(regoffset, 1).addr();
  const zx_off_t erdp_addr = ERDP::Get(regoffset, 1).addr();
  libsync::Completion entered_starting;
  libsync::Completion release_starting;
  std::atomic<zx_status_t> start_status{ZX_ERR_INTERNAL};
  bool start_posted = false;
  libsync::Completion start_done;
  libsync::Completion stop_done;
  std::thread stop_thread;

  auto cleanup = fit::defer([&] {
    release_starting.Signal();
    if (stop_thread.joinable()) {
      stop_thread.join();
    }
    if (start_posted) {
      start_done.Wait();
    }
    interrupter.Stop();
    driver_test().runtime().ShutdownBackgroundDispatcher(start_dispatcher->get(), [] {});
    mmio_region()[erdp_addr].SetReadCallback([] { return 0u; });
    mmio_region()[iman_addr].SetReadCallback([] { return 0u; });
    mmio_region()[iman_addr].SetWriteCallback([](uint64_t) {});
  });

  mmio_region()[iman_addr].SetReadCallback(
      [&iman_val] { return iman_val.load(std::memory_order_acquire); });
  mmio_region()[iman_addr].SetWriteCallback([&iman_val](uint64_t val) {
    iman_val.store(static_cast<uint32_t>(val), std::memory_order_release);
  });
  mmio_region()[erdp_addr].SetReadCallback([&entered_starting, &release_starting] {
    entered_starting.Signal();
    release_starting.Wait();
    return 0u;
  });

  ASSERT_OK(async::PostTask(start_dispatcher->async_dispatcher(), [this, &interrupter, regoffset,
                                                                   &start_status, &start_done] {
    start_status.store(interrupter.Start(regoffset, mmio_buffer()->View(0)),
                       std::memory_order_release);
    start_done.Signal();
  }));
  start_posted = true;
  entered_starting.Wait();

  // While paused in StartingState, concurrent Start() fails and Timeout() rejects immediately.
  EXPECT_STATUS(interrupter.Start(regoffset, mmio_buffer()->View(0)), ZX_ERR_BAD_STATE);
  EXPECT_STATUS(RunImmediateTimeout(interrupter, zx::time::infinite()), ZX_ERR_IO_NOT_PRESENT);

  // A concurrent Stop() records stop_requested and waits until Start() aborts and tears down its
  // unused dispatcher.
  stop_thread = std::thread([&interrupter, &stop_done] {
    interrupter.Stop();
    stop_done.Signal();
  });
  while (!InterrupterTestPeer::StartingStopRequested(interrupter) && !stop_done.signaled()) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  ASSERT_TRUE(InterrupterTestPeer::StartingStopRequested(interrupter));
  EXPECT_FALSE(stop_done.signaled());

  release_starting.Signal();
  start_done.Wait();
  stop_done.Wait();
  stop_thread.join();

  EXPECT_STATUS(start_status.load(std::memory_order_acquire), ZX_ERR_BAD_STATE);
  EXPECT_FALSE(interrupter.active());
  EXPECT_EQ(IMAN::Get(regoffset, 1).FromValue(iman_val.load(std::memory_order_acquire)).IE(), 0u);

  // After the aborted Start() + Stop() serialize back to std::monostate, a subsequent Start()
  // succeeds cleanly.
  mmio_region()[erdp_addr].SetReadCallback([] { return 0u; });
  ASSERT_OK(interrupter.Start(regoffset, mmio_buffer()->View(0)));
  ASSERT_OK(InterrupterTestPeer::Flush(interrupter));
  EXPECT_TRUE(interrupter.active());
  EXPECT_EQ(IMAN::Get(regoffset, 1).FromValue(iman_val.load(std::memory_order_acquire)).IE(), 1u);
  interrupter.Stop();
}

}  // namespace

}  // namespace usb_xhci
