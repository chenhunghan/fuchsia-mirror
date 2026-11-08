// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/xhci-event-ring.h"

#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>

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

class EventRingHarness : public ::testing::Test {
 public:
  EventRingHarness() : trb_context_allocator_(-1, true) {}
  void SetUp() override {
    // Globals
    constexpr auto kRuntimeRegisterOffset = 6 * sizeof(uint32_t);
    constexpr auto kErdp = 2062 * sizeof(uint32_t);
    region_.emplace(sizeof(uint32_t), 4096);
    buffer_.emplace(region_->GetMmioBuffer());

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
    });
    ASSERT_TRUE(driver_test()
                    .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                      xhci_config::Config fake_config;
                      fake_config.enable_suspend() = false;
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
    ring_ = ring;
    auto regoffset = RuntimeRegisterOffset::Get().ReadFrom(&buffer_.value());
    zx_status_t status =
        ring->Init(zx_system_get_page_size(), kFakeBti, &buffer_.value(), false, kErstMax,
                   ERSTSZ::Get(regoffset, 0).ReadFrom(&buffer_.value()),
                   ERDP::Get(regoffset, 0).ReadFrom(&buffer_.value()),
                   IMAN::Get(regoffset, 0).ReadFrom(&buffer_.value()),
                   CapLength::Get().ReadFrom(&buffer_.value()).Length(),
                   HCSPARAMS1::Get().ReadFrom(&buffer_.value()), &command_ring_,
                   DoorbellOffset::Get().ReadFrom(&buffer_.value()), driver_test().driver(),
                   HCCPARAMS1::Get().ReadFrom(&buffer_.value()), dcbaa_, 0, nullptr);
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
  void SetShortPacketHandler(fit::function<void(usb_xhci::TRB*, size_t, usb_xhci::TRB**)> handler) {
    short_packet_handler_ = std::move(handler);
  }

  void HandleShortPacket(TRB* short_trb, size_t short_length, TRB** first_trb) {
    (*short_packet_handler_)(short_trb, short_length, first_trb);
  }

  zx_status_t CompleteTRB(TRB* trb, std::unique_ptr<TRBContext>* context) {
    if (trb != expected_completion_) {
      return ZX_ERR_IO;
    }
    *context = pending_contexts_.pop_front();
    return ZX_OK;
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
    auto& state = driver_test().driver()->GetDeviceState()[i - 1];
    ASSERT_TRUE(state);
    fbl::AutoLock _(&state->transaction_lock());
    state->SetDeviceInformation(i, 1, std::nullopt);
  }

  fdf_testing::ForegroundDriverTest<EmptyTestConfig>& driver_test() { return driver_test_; }
  PORTSC& port_sc() { return port_sc_; }
  PORTSC& second_port_sc() { return second_port_sc_; }
  size_t port_sc_write_count() const { return port_sc_write_count_; }
  size_t port_sc_read_count() const { return port_sc_read_count_; }
  void SetPortScWriteHook(fit::function<void(const PORTSC&)> hook) {
    port_sc_write_hook_ = std::move(hook);
  }
  void SetPortScReadHook(fit::function<void()> hook) { port_sc_read_hook_ = std::move(hook); }
  EventRing* event_ring() { return ring_; }

 private:
  fdf_testing::ForegroundDriverTest<EmptyTestConfig> driver_test_;
  std::optional<Request> pending_req_;
  std::optional<fit::function<void(usb_xhci::TRB*, size_t, usb_xhci::TRB**)>> short_packet_handler_;
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
};

// UsbXhci Methods
bool UsbXhci::Running() const { return running_; }

zx::result<> UsbXhci::Init(std::unique_ptr<dma_buffer::BufferFactory> buffer_factory) {
  buffer_factory_ = std::move(buffer_factory);

  fbl::AllocChecker ac;
  interrupters_ = fbl::MakeArray<Interrupter>(&ac, 1);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  max_slots_ = 32;
  device_state_ = fbl::MakeArray<fbl::RefPtr<DeviceState>>(&ac, max_slots_);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  for (size_t i = 0; i < max_slots_; i++) {
    device_state_[i] = fbl::MakeRefCounted<DeviceState>(static_cast<uint32_t>(i), this);
    fbl::AutoLock l(&device_state_[i]->transaction_lock());
    for (size_t c = 0; c < max_slots_; c++) {
      device_state_[i]->InitEndpoint(static_cast<uint8_t>(c), nullptr, nullptr);
    }
  }
  port_state_ = fbl::MakeArray<PortState>(&ac, 32);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  params_.set_MaxPorts(32);
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

void UsbXhci::Shutdown(zx_status_t status) {}

// TransferRing Methods
zx_status_t TransferRing::Init(size_t page_size, const zx::bti& bti, EventRing* ring, bool is_32bit,
                               fdf::MmioBuffer* mmio, UsbXhci* hci) {
  fbl::AutoLock _(&mutex_);
  hci_ = hci;
  return ZX_OK;
}

zx_status_t TransferRing::HandleShortPacket(TRB* short_trb, size_t short_length, TRB** first_trb) {
  hci_->GetTestHarness<EventRingHarness>()->HandleShortPacket(short_trb, short_length, first_trb);
  return ZX_OK;
}

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBsUntil(TRB* end) {
  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> empty;
  return empty;
}

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBs() {
  fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> empty;
  return empty;
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
  SetCompletion(reinterpret_cast<TRB*>(kFakeTrbVirt));
  Interrupt();
  ASSERT_EQ(transfer_status, ZX_ERR_IO_REFUSED);
  ASSERT_EQ(transfer_len, 0UL);
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
  auto state = driver_test().driver()->GetDeviceState()[0];
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
  auto state = driver_test().driver()->GetDeviceState()[0];
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

}  // namespace

}  // namespace usb_xhci
