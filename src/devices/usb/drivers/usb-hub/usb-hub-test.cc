// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-hub/usb-hub.h"

#include <fidl/fuchsia.hardware.usb.descriptor/cpp/fidl.h>
#include <fuchsia/hardware/usb/hub/c/banjo.h>
#include <fuchsia/hardware/usb/request/c/banjo.h>
#include <lib/fit/defer.h>
#include <lib/stdcompat/span.h>
#include <lib/synchronous-executor/executor.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>
#include <string.h>
#include <unistd.h>
#include <zircon/process.h>
#include <zircon/time.h>

#include <array>
#include <atomic>
#include <cstring>
#include <functional>
#include <thread>
#include <variant>

#include <fbl/condition_variable.h>
#include <fbl/ref_ptr.h>
#include <usb/descriptors.h>
#include <zxtest/zxtest.h>

#include "lib/fpromise/promise.h"
#include "src/devices/testing/mock-ddk/mock-device.h"
#include "src/devices/usb/drivers/usb-hub/fake-device.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace {
template <EmulationMode mode>
class UsbHarness : public zxtest::Test {
 public:
  void SetUp() override {
    parent_ = MockDevice::FakeRootParent();
    device_.emplace(mode);

    usb_protocol_t usb_proto;
    usb_bus_protocol_t bus_proto;
    ASSERT_OK(device_->GetProtocol(ZX_PROTOCOL_USB, &usb_proto));
    ASSERT_OK(device_->GetProtocol(ZX_PROTOCOL_USB_BUS, &bus_proto));
    parent_->AddProtocol(ZX_PROTOCOL_USB, usb_proto.ops, usb_proto.ctx);
    parent_->AddProtocol(ZX_PROTOCOL_USB_BUS, bus_proto.ops, bus_proto.ctx);

    usb_hub::UsbHubDevice::Bind(nullptr, reinterpret_cast<zx_device_t*>(parent_.get()));

    device_->SetChildContext(parent_->GetLatestChild()->GetDeviceContext<usb_hub::UsbHubDevice>());
    device()->DdkInit(ddk::InitTxn(device()->zxdev()));
    parent_->GetLatestChild()->WaitUntilInitReplyCalled();
    device_->InitComplete();
    ASSERT_TRUE(device_->HasOps());

    auto& queue = device_->GetStateChangeQueue();
    bool powered_on = false;
    bool initialized = false;
    while (!(powered_on && initialized)) {
      auto entry = queue.Wait();
      switch (entry->type) {
        case OperationType::kPowerOnEvent:
          powered_on = true;
          break;
        case OperationType::kInitCompleteEvent:
          initialized = true;
          break;
        default:
          abort();
      }
    }
  }

  auto StartDispatching() {
    dispatching_ = true;
    device_->GetStateChangeQueue().StartThread(fit::bind_member(this, &UsbHarness::DispatchThread));
    return fit::defer([this]() { StopDispatching(); });
  }

  void SetConnectCallback(fit::function<zx_status_t(uint32_t port, usb_speed_t speed)> callback) {
    connect_callback_ = std::move(callback);
  }

  void DispatchThread() {
    while (dispatching_) {
      auto entry = device_->GetStateChangeQueue().Wait();
      switch (entry->type) {
        case OperationType::kUsbBusDeviceAdded:
          entry->status = connect_callback_(entry->port, entry->speed);
          Complete(std::move(entry));
          break;
        case OperationType::kUsbBusDeviceRemoved:
          entry->status = connect_callback_(entry->port, -1);
          Complete(std::move(entry));
          break;
        case OperationType::kExitEventLoop:
          return;
        default:
          abort();
      }
    }
  }

  void ConnectDevice(uint8_t port, usb_speed_t speed) { device_->ConnectDevice(port, speed); }

  void DisconnectDevice(uint8_t port) { device_->DisconnectDevice(port); }

  zx_status_t ResetPort(uint8_t port) { return device_->ResetPort(port); }

  bool ResetPending(uint8_t port) { return device_->ResetPending(port); }

  void Interrupt() { device_->Interrupt(); }
  usb_hub::UsbHubDevice* device() { return device_->device(); }
  FakeDevice* fake_device() { return &*device_; }

  void TearDown() override {
    device()->DdkUnbind(ddk::UnbindTxn(device()->zxdev()));
    parent_->GetLatestChild()->WaitUntilUnbindReplyCalled();
    ASSERT_EQ(device_->cancel_all_count(), 1u);
    device_->Unplug();
  }

 private:
  std::shared_ptr<MockDevice> parent_;

  void StopDispatching() {
    dispatching_ = false;
    device_->GetStateChangeQueue().Insert(MakeSyncEntry(OperationType::kExitEventLoop));
    device_->GetStateChangeQueue().Join();
  }
  std::atomic_bool dispatching_ = false;
  std::optional<FakeDevice> device_;
  fit::function<zx_status_t(uint32_t port, usb_speed_t speed)> connect_callback_;
};

// The SyntheticHarness tests currently assume DdkInit() has not been invoked.
class SyntheticHarness : public zxtest::Test {
 public:
  SyntheticHarness() : loop_(&kAsyncLoopConfigNeverAttachToThread) {}
  void SetUp() override {
    zx_status_t status = loop_.StartThread();
    if (status != ZX_OK) {
      return;
    }
    auto executor = std::make_unique<async::Executor>(loop_.dispatcher());
    executor_ = executor.get();

    parent_ = MockDevice::FakeRootParent();
    device_.emplace(EmulationMode::Smays);
    device_->SetSynthetic(true);

    usb_protocol_t usb_proto;
    usb_bus_protocol_t bus_proto;
    ASSERT_OK(device_->GetProtocol(ZX_PROTOCOL_USB, &usb_proto));
    ASSERT_OK(device_->GetProtocol(ZX_PROTOCOL_USB_BUS, &bus_proto));
    parent_->AddProtocol(ZX_PROTOCOL_USB, usb_proto.ops, usb_proto.ctx);
    parent_->AddProtocol(ZX_PROTOCOL_USB_BUS, bus_proto.ops, bus_proto.ctx);

    usb_hub::UsbHubDevice::Bind(std::move(executor), reinterpret_cast<zx_device_t*>(parent_.get()));
    device_->SetChildContext(parent_->GetLatestChild()->GetDeviceContext<usb_hub::UsbHubDevice>());
    ASSERT_TRUE(device_->HasOps());
  }

  void SetRequestCallback(
      fit::function<void(usb_request_t*, usb_request_complete_callback_t)> callback) {
    device_->SetRequestCallback(std::move(callback));
  }

  usb_hub::UsbHubDevice* device() { return device_->device(); }

  void TearDown() override {
    device()->DdkUnbind(ddk::UnbindTxn(device()->zxdev()));
    parent_->GetLatestChild()->WaitUntilUnbindReplyCalled();
    loop_.Shutdown();
  }

 private:
  std::shared_ptr<MockDevice> parent_;
  std::optional<FakeDevice> device_;
  async::Loop loop_;
  fpromise::executor* executor_;
};

class SmaysHarness : public UsbHarness<EmulationMode::Smays> {
 public:
};

class UnbrandedHarness : public UsbHarness<EmulationMode::Unbranded> {
 public:
};

TEST_F(SmaysHarness, Usb2Hub) {
  auto dispatcher = StartDispatching();
  // Enumeration might not happen in port order.
  // See USB 2.0 specification revision 2.0 section 9.1.2.
  sync_completion_t enum_complete;
  uint8_t port_bitmask = 7;
  usb_speed_t speeds[] = {USB_SPEED_HIGH, USB_SPEED_LOW, USB_SPEED_FULL};

  SetConnectCallback([&enum_complete, &speeds, &port_bitmask](uint32_t port, usb_speed_t speed) {
    ZX_ASSERT((speed - 1) < std::size(speeds));
    ZX_ASSERT(speed < sizeof(int));
    if ((speed != speeds[port - 1])) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (!(port_bitmask & (1 << (port - 1)))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_bitmask &= ~(1 << (port - 1));
    if (!port_bitmask) {
      sync_completion_signal(&enum_complete);
    }
    return ZX_OK;
  });

  ConnectDevice(0, USB_SPEED_HIGH);
  ConnectDevice(1, USB_SPEED_LOW);
  ConnectDevice(2, USB_SPEED_FULL);
  Interrupt();
  sync_completion_wait(&enum_complete, ZX_TIME_INFINITE);
  sync_completion_t disconnect_complete;
  // Disconnect ordering doesn't matter (can happen in any order).
  uint8_t port_remove_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if ((speed != static_cast<usb_speed_t>(-1))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_remove_bitmask &= ~(1 << (port - 1));
    if (port_remove_bitmask == 0) {
      sync_completion_signal(&disconnect_complete);
    }
    return ZX_OK;
  });
  DisconnectDevice(0);
  DisconnectDevice(1);
  DisconnectDevice(2);
  Interrupt();
  sync_completion_wait(&disconnect_complete, ZX_TIME_INFINITE);
  ASSERT_OK(ResetPort(1));
  ASSERT_TRUE(ResetPending(1));
}

TEST_F(UnbrandedHarness, Usb3Hub) {
  auto dispatcher = StartDispatching();
  sync_completion_t enum_complete;
  sync_completion_reset(&enum_complete);
  uint8_t port_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if (speed != USB_SPEED_SUPER) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (!(port_bitmask & (1 << (port - 1)))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_bitmask &= ~(1 << (port - 1));
    if (!port_bitmask) {
      sync_completion_signal(&enum_complete);
    }
    return ZX_OK;
  });

  ConnectDevice(0, USB_SPEED_SUPER);
  ConnectDevice(1, USB_SPEED_SUPER);
  ConnectDevice(2, USB_SPEED_SUPER);
  Interrupt();
  sync_completion_wait(&enum_complete, ZX_TIME_INFINITE);
  sync_completion_t disconnect_complete;
  // Disconnect ordering doesn't matter (can happen in any order).
  uint8_t port_remove_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if ((speed != static_cast<usb_speed_t>(-1))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_remove_bitmask &= ~(1 << (port - 1));
    if (port_remove_bitmask == 0) {
      sync_completion_signal(&disconnect_complete);
    }
    return ZX_OK;
  });
  DisconnectDevice(0);
  DisconnectDevice(1);
  DisconnectDevice(2);
  Interrupt();
  sync_completion_wait(&disconnect_complete, ZX_TIME_INFINITE);
  ASSERT_OK(ResetPort(1));
  ASSERT_TRUE(ResetPending(1));
}

TEST_F(UnbrandedHarness, Usb3HubReenumerate) {
  auto dispatcher = StartDispatching();
  sync_completion_t enum_complete;
  sync_completion_reset(&enum_complete);
  uint8_t port_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if (speed != USB_SPEED_SUPER) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (!(port_bitmask & (1 << (port - 1)))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_bitmask &= ~(1 << (port - 1));
    if (!port_bitmask) {
      sync_completion_signal(&enum_complete);
    }
    return ZX_OK;
  });

  ConnectDevice(0, USB_SPEED_SUPER);
  ConnectDevice(1, USB_SPEED_SUPER);
  ConnectDevice(2, USB_SPEED_SUPER);
  Interrupt();
  sync_completion_wait(&enum_complete, ZX_TIME_INFINITE);

  // Re-enumerate port 0.
  sync_completion_t reenum_complete;
  sync_completion_reset(&reenum_complete);
  bool port0_enumerated = true;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if (port0_enumerated) {
      port0_enumerated = false;
      return speed == static_cast<usb_speed_t>(-1) ? ZX_OK : ZX_ERR_INVALID_ARGS;
    }

    sync_completion_signal(&reenum_complete);
    return speed == USB_SPEED_SUPER ? ZX_OK : ZX_ERR_INVALID_ARGS;
  });

  ConnectDevice(0, USB_SPEED_SUPER);
  Interrupt();
  sync_completion_wait(&reenum_complete, ZX_TIME_INFINITE);

  sync_completion_t disconnect_complete;
  // Disconnect ordering doesn't matter (can happen in any order).
  uint8_t port_remove_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if ((speed != static_cast<usb_speed_t>(-1))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_remove_bitmask &= ~(1 << (port - 1));
    if (port_remove_bitmask == 0) {
      sync_completion_signal(&disconnect_complete);
    }
    return ZX_OK;
  });
  DisconnectDevice(0);
  DisconnectDevice(1);
  DisconnectDevice(2);
  Interrupt();
  sync_completion_wait(&disconnect_complete, ZX_TIME_INFINITE);
  ASSERT_OK(ResetPort(1));
  ASSERT_TRUE(ResetPending(1));
}

TEST_F(UnbrandedHarness, UnbindHubDisconnects) {
  auto dispatcher = StartDispatching();
  sync_completion_t enum_complete;
  sync_completion_reset(&enum_complete);
  uint8_t port_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if (speed != USB_SPEED_SUPER) {
      return ZX_ERR_INVALID_ARGS;
    }
    if (!(port_bitmask & (1 << (port - 1)))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_bitmask &= ~(1 << (port - 1));
    if (!port_bitmask) {
      sync_completion_signal(&enum_complete);
    }
    return ZX_OK;
  });

  ConnectDevice(0, USB_SPEED_SUPER);
  ConnectDevice(1, USB_SPEED_SUPER);
  ConnectDevice(2, USB_SPEED_SUPER);
  Interrupt();
  sync_completion_wait(&enum_complete, ZX_TIME_INFINITE);
  sync_completion_t disconnect_complete;
  // Disconnect ordering doesn't matter (can happen in any order).
  uint8_t port_remove_bitmask = 7;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    if ((speed != static_cast<usb_speed_t>(-1))) {
      return ZX_ERR_INVALID_ARGS;
    }
    port_remove_bitmask &= ~(1 << (port - 1));
    if (port_remove_bitmask == 0) {
      sync_completion_signal(&disconnect_complete);
    }
    return ZX_OK;
  });

  ASSERT_OK(device()->Shutdown());
  sync_completion_wait(&disconnect_complete, ZX_TIME_INFINITE);
}

TEST_F(SyntheticHarness, SetFeature) {
  auto dev = device();
  bool ran = false;
  constexpr uint8_t kRequestType = fdescriptor::EndpointDirection::kOut |
                                   fdescriptor::RequestType::kStandard |
                                   fdescriptor::RequestRecipient::kOther;
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    ASSERT_EQ(request->setup.bm_request_type, kRequestType);
    ASSERT_EQ(request->setup.b_request,
              fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature));
    ASSERT_EQ(request->setup.w_index, 2);
    ran = true;
    usb_request_complete(request, ZX_OK, 0, &completion);
  });
  ASSERT_OK(dev->SetFeature(kRequestType, 7, 2));
  ASSERT_TRUE(ran);
}

TEST_F(SyntheticHarness, ClearFeature) {
  auto dev = device();
  bool ran = false;
  constexpr uint8_t kRequestType = fdescriptor::EndpointDirection::kOut |
                                   fdescriptor::RequestType::kStandard |
                                   fdescriptor::RequestRecipient::kOther;
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    ASSERT_EQ(request->setup.bm_request_type, kRequestType);
    ASSERT_EQ(request->setup.b_request,
              fidl::ToUnderlying(fdescriptor::StandardRequest::kClearFeature));
    ASSERT_EQ(request->setup.w_index, 2);
    ran = true;
    usb_request_complete(request, ZX_OK, 0, &completion);
  });
  ASSERT_OK(dev->ClearFeature(kRequestType, 7, 2));
  ASSERT_TRUE(ran);
}

TEST_F(SyntheticHarness, GetPortStatus) {
  auto dev = device();
  // Run through all 127 permutations of port configuration states
  // and ensure we set the correct bits for each one.
  for (uint16_t i = 0; i < 127; i++) {
    uint16_t features_cleared = 0;
    SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
      switch (request->setup.bm_request_type) {
        case kClassPortIn: {
          usb_port_status_t* stat;
          usb_request_mmap(request, reinterpret_cast<void**>(&stat));
          stat->w_port_change = i;
          usb_request_complete(request, ZX_OK, sizeof(usb_port_status_t), &completion);
          return;
        } break;
        case kClassPortOut: {
          switch (request->setup.w_value) {
            case USB_FEATURE_C_PORT_CONNECTION:
              features_cleared |= USB_C_PORT_CONNECTION;
              break;
            case USB_FEATURE_C_PORT_ENABLE:
              features_cleared |= USB_C_PORT_ENABLE;
              break;
            case USB_FEATURE_C_PORT_SUSPEND:
              features_cleared |= USB_C_PORT_SUSPEND;
              break;
            case USB_FEATURE_C_PORT_OVER_CURRENT:
              features_cleared |= USB_C_PORT_OVER_CURRENT;
              break;
            case USB_FEATURE_C_PORT_RESET:
              features_cleared |= USB_C_PORT_RESET;
              break;
            case USB_FEATURE_C_BH_PORT_RESET:
              features_cleared |= USB_C_BH_PORT_RESET;
              break;
            case USB_FEATURE_C_PORT_LINK_STATE:
              features_cleared |= USB_C_PORT_LINK_STATE;
              break;
            case USB_FEATURE_C_PORT_CONFIG_ERROR:
              features_cleared |= USB_C_PORT_CONFIG_ERROR;
              break;
          }
        } break;
        default:
          ASSERT_TRUE(false);
      }
      usb_request_complete(request, ZX_OK, 0, &completion);
    });
    auto result = dev->GetPortStatus(usb_hub::PortNumber(static_cast<uint8_t>(i)));
    usb_port_status_t port_status = result.value();
    ASSERT_EQ(port_status.w_port_change, i);
    ASSERT_EQ(features_cleared, i);
  }
}

// This test checks that GetUsbHubDescriptor will fail on invalid sized descriptors
TEST_F(SyntheticHarness, BadDescriptorTest) {
  auto dev = device();
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_device_descriptor_t* devdesc;
    usb_request_mmap(request, reinterpret_cast<void**>(&devdesc));
    devdesc->b_length = sizeof(usb_device_descriptor_t);
    usb_request_complete(request, ZX_OK, sizeof(usb_descriptor_header_t), &completion);
  });
  auto result = dev->GetUsbHubDescriptor(0);
  ASSERT_EQ(result.error_value(), ZX_ERR_IO_REFUSED);

  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_device_descriptor_t* devdesc;
    usb_request_mmap(request, reinterpret_cast<void**>(&devdesc));
    devdesc->b_length = sizeof(usb_device_descriptor_t);
    usb_request_complete(request, ZX_OK, sizeof(usb_hub_descriptor_t), &completion);
  });
  result = dev->GetUsbHubDescriptor(0);
  ASSERT_EQ(result.error_value(), ZX_ERR_BUFFER_TOO_SMALL);
}

TEST_F(SyntheticHarness, ExceedsStructCapacityTest) {
  auto dev = device();
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_hub_descriptor_t* hubdesc;
    usb_request_mmap(request, reinterpret_cast<void**>(&hubdesc));
    hubdesc->b_desc_length = 30;
    hubdesc->b_nbr_ports = 128;
    usb_request_complete(request, ZX_OK, 30, &completion);
  });
  auto result = dev->GetUsbHubDescriptor(0);
  ASSERT_EQ(result.error_value(), ZX_ERR_BUFFER_TOO_SMALL);
}

TEST_F(SyntheticHarness, MismatchedHubDescriptorLengthTest) {
  auto dev = device();
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_hub_descriptor_t* hubdesc;
    usb_request_mmap(request, reinterpret_cast<void**>(&hubdesc));
    hubdesc->b_desc_length = 20;  // Claims 20 bytes, but request_size is 9 bytes
    hubdesc->b_nbr_ports = 4;
    usb_request_complete(request, ZX_OK, 9, &completion);
  });
  auto result = dev->GetUsbHubDescriptor(0);
  ASSERT_EQ(result.error_value(), ZX_ERR_BUFFER_TOO_SMALL);
}

TEST_F(SyntheticHarness, GoodDescriptorTest) {
  auto dev = device();
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_device_descriptor_t* devdesc;
    usb_request_mmap(request, reinterpret_cast<void**>(&devdesc));
    devdesc->b_length = sizeof(usb_hub_descriptor_t);
    usb_request_complete(request, ZX_OK, sizeof(usb_hub_descriptor_t), &completion);
  });
  auto result =
      dev->GetUsbHubDescriptor(fidl::ToUnderlying(fdescriptor::DescriptorType::kHubSuperSpeed));
  ASSERT_OK(result);

  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_hub_descriptor_t* hubdesc;
    usb_request_mmap(request, reinterpret_cast<void**>(&hubdesc));
    hubdesc->b_desc_length = 9;
    hubdesc->b_nbr_ports = 4;
    usb_request_complete(request, ZX_OK, 9, &completion);
  });
  result = dev->GetUsbHubDescriptor(0);
  ASSERT_OK(result);
}

TEST_F(SyntheticHarness, ControlInErrorPropagationTest) {
  auto dev = device();
  SetRequestCallback([&](usb_request_t* request, usb_request_complete_callback_t completion) {
    usb_request_complete(request, ZX_ERR_IO, 0, &completion);
  });
  auto result = dev->GetUsbHubDescriptor(0);
  ASSERT_TRUE(result.is_error());
  ASSERT_EQ(result.error_value(), ZX_ERR_IO);
}

class InitErrorHarness : public zxtest::Test {
 public:
  void SetUp() override {
    parent_ = MockDevice::FakeRootParent();
    device_.emplace(EmulationMode::Smays);

    usb_protocol_t usb_proto;
    usb_bus_protocol_t bus_proto;
    ASSERT_OK(device_->GetProtocol(ZX_PROTOCOL_USB, &usb_proto));
    ASSERT_OK(device_->GetProtocol(ZX_PROTOCOL_USB_BUS, &bus_proto));
    parent_->AddProtocol(ZX_PROTOCOL_USB, usb_proto.ops, usb_proto.ctx);
    parent_->AddProtocol(ZX_PROTOCOL_USB_BUS, bus_proto.ops, bus_proto.ctx);

    ASSERT_OK(usb_hub::UsbHubDevice::Bind(nullptr, reinterpret_cast<zx_device_t*>(parent_.get())));
    device_->SetChildContext(parent_->GetLatestChild()->GetDeviceContext<usb_hub::UsbHubDevice>());
    ASSERT_TRUE(device_->HasOps());
  }

  void TearDown() override {
    device()->DdkUnbind(ddk::UnbindTxn(device()->zxdev()));
    parent_->GetLatestChild()->WaitUntilUnbindReplyCalled();
    ASSERT_EQ(device_->cancel_all_count(), expected_cancel_all_count_);
    device_->Unplug();
  }

  void set_expected_cancel_all_count(uint32_t count) { expected_cancel_all_count_ = count; }
  MockDevice* child_mock() { return parent_->GetLatestChild(); }
  usb_hub::UsbHubDevice* device() { return device_->device(); }
  FakeDevice* fake_device() { return &*device_; }

 private:
  std::shared_ptr<MockDevice> parent_;
  std::optional<FakeDevice> device_;
  uint32_t expected_cancel_all_count_ = 0;
};

TEST_F(InitErrorHarness, DdkInitFailsWhenNoInterruptEndpoint) {
  // Configuration descriptor with three hub interfaces: one with b_num_endpoints == 0, one with a
  // single Bulk IN endpoint (exercising !usb_ep_is_int), and one with a single Interrupt OUT
  // endpoint (exercising !usb_ep_is_in).
  static constexpr uint8_t kNoEndpointHubDescriptor[] = {
      9, 2, 50,   0, 3, 1, 0,  224, 50,  // Configuration descriptor (3 interfaces, 50 bytes)
      9, 4, 0,    0, 0, 9, 0,  0,   0,   // Interface 0 (b_num_endpoints = 0)
      9, 4, 1,    0, 1, 9, 0,  0,   0,   // Interface 1 (b_num_endpoints = 1)
      7, 5, 0x81, 2, 8, 0, 0,            // Bulk IN endpoint (0x81, bm_attributes = 2)
      9, 4, 2,    0, 1, 9, 0,  0,   0,   // Interface 2 (b_num_endpoints = 1)
      7, 5, 0x01, 3, 8, 0, 12,           // Interrupt OUT endpoint (0x01, bm_attributes = 3)
  };
  fake_device()->SetDescriptorOverride(
      cpp20::span(kNoEndpointHubDescriptor, sizeof(kNoEndpointHubDescriptor)));

  device()->DdkInit(ddk::InitTxn(device()->zxdev()));
  child_mock()->WaitUntilInitReplyCalled();
  ASSERT_EQ(child_mock()->InitReplyCallStatus(), ZX_ERR_IO);
  ASSERT_EQ(fake_device()->configure_hub_seq(), 0u);
  ASSERT_EQ(fake_device()->enable_endpoint_seq(), 0u);
  ASSERT_EQ(fake_device()->power_on_seq(), 0u);
}

TEST_F(InitErrorHarness, DdkInitFailsWhenConfigureHubFails) {
  fake_device()->SetConfigureHubStatus(ZX_ERR_IO);
  set_expected_cancel_all_count(1u);

  device()->DdkInit(ddk::InitTxn(device()->zxdev()));
  child_mock()->WaitUntilInitReplyCalled();
  ASSERT_EQ(child_mock()->InitReplyCallStatus(), ZX_ERR_IO);
  ASSERT_GT(fake_device()->enable_endpoint_seq(), 0u);
  ASSERT_GT(fake_device()->set_hub_interface_seq(), fake_device()->enable_endpoint_seq());
  ASSERT_GT(fake_device()->configure_hub_seq(), fake_device()->set_hub_interface_seq());
  ASSERT_EQ(fake_device()->power_on_seq(), 0u);
}

TEST_F(InitErrorHarness, DdkInitFailsWhenEnableEndpointFails) {
  fake_device()->SetEnableEndpointStatus(ZX_ERR_IO_REFUSED);

  device()->DdkInit(ddk::InitTxn(device()->zxdev()));
  child_mock()->WaitUntilInitReplyCalled();
  ASSERT_EQ(child_mock()->InitReplyCallStatus(), ZX_ERR_IO_REFUSED);
  ASSERT_GT(fake_device()->enable_endpoint_seq(), 0u);
  ASSERT_EQ(fake_device()->set_hub_interface_seq(), 0u);
  ASSERT_EQ(fake_device()->configure_hub_seq(), 0u);
  ASSERT_EQ(fake_device()->power_on_seq(), 0u);
}

TEST_F(SmaysHarness, UnbindRepliesWhenCancelAllFails) {
  // SmaysHarness::TearDown() invokes DdkUnbind() and WaitUntilUnbindReplyCalled(), verifying that
  // Shutdown() still replies to unbind_txn_ even when CancelAll() returns an error.
  fake_device()->SetCancelAllStatus(ZX_ERR_IO);
}

TEST_F(SmaysHarness, FailedDeviceAddedDisablesPortClearsTtBufferAndContinuesEnumeration) {
  auto dispatcher = StartDispatching();
  sync_completion_t enum_complete;
  uint8_t attempted_ports = 0;

  // Port 1 (Low-Speed) and Port 2 (High-Speed) fail DeviceAdded; Port 3 (Full-Speed) succeeds.
  // Because HandleResetComplete disables the port and clears the TT buffer before calling
  // EnumerateNext, Ports 1 and 2 have already finished cleanup by the time Port 3 runs.
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    attempted_ports |= static_cast<uint8_t>(1u << (port - 1));
    if (port == 1) {
      ZX_ASSERT(speed == USB_SPEED_LOW);
      return ZX_ERR_IO;
    }
    if (port == 2) {
      ZX_ASSERT(speed == USB_SPEED_HIGH);
      return ZX_ERR_IO;
    }
    ZX_ASSERT(port == 3);
    ZX_ASSERT(speed == USB_SPEED_FULL);
    ZX_ASSERT(attempted_ports == 0b111);
    sync_completion_signal(&enum_complete);
    return ZX_OK;
  });

  ConnectDevice(0, USB_SPEED_LOW);
  ConnectDevice(1, USB_SPEED_HIGH);
  ConnectDevice(2, USB_SPEED_FULL);
  Interrupt();
  ASSERT_OK(sync_completion_wait(&enum_complete, ZX_TIME_INFINITE));

  ASSERT_EQ(fake_device()->clear_port_enable_count(), 2u);
  ASSERT_FALSE(fake_device()->PortEnabled(0));
  ASSERT_EQ(fake_device()->PortChangeMask(0), 0u);
  ASSERT_FALSE(fake_device()->PortEnabled(1));
  ASSERT_EQ(fake_device()->PortChangeMask(1), 0u);
  ASSERT_TRUE(fake_device()->PortEnabled(2));

  // CLEAR_TT_BUFFER should be issued only for the failed Low-Speed port (EP0 OUT 0x0000 and EP0 IN
  // 0x8000 on Single-TT wIndex=1), not for the failed High-Speed port.
  auto tt_calls = fake_device()->GetClearTtBufferCalls();
  ASSERT_EQ(tt_calls.size(), 2u);
  ASSERT_EQ(tt_calls[0].value, 0x0000u);
  ASSERT_EQ(tt_calls[0].index, 1u);
  ASSERT_EQ(tt_calls[1].value, 0x8000u);
  ASSERT_EQ(tt_calls[1].index, 1u);

  // Disconnecting all three ports should only invoke DeviceRemoved for Port 3 (the only port whose
  // DeviceAdded succeeded).
  sync_completion_t disconnect_complete;
  uint32_t removed_ports_mask = 0;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    ZX_ASSERT(speed == static_cast<usb_speed_t>(-1));
    removed_ports_mask |= (1u << (port - 1));
    sync_completion_signal(&disconnect_complete);
    return ZX_OK;
  });
  DisconnectDevice(0);
  DisconnectDevice(1);
  DisconnectDevice(2);
  Interrupt();
  ASSERT_OK(sync_completion_wait(&disconnect_complete, ZX_TIME_INFINITE));
  ASSERT_EQ(removed_ports_mask, 0b100u);
}

TEST_F(UnbrandedHarness, SuperSpeedFailedDeviceAddedDoesNotClearPortEnable) {
  auto dispatcher = StartDispatching();
  sync_completion_t enum_complete;

  // Connect Port 1 and Port 2 so that when Port 2's connect callback runs via EnumerateNext(),
  // Port 1's HandleResetComplete error-handling block has already finished.
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    ZX_ASSERT(speed == USB_SPEED_SUPER);
    if (port == 1) {
      return ZX_ERR_IO;
    }
    ZX_ASSERT(port == 2);
    sync_completion_signal(&enum_complete);
    return ZX_OK;
  });

  ConnectDevice(0, USB_SPEED_SUPER);
  ConnectDevice(1, USB_SPEED_SUPER);
  Interrupt();
  ASSERT_OK(sync_completion_wait(&enum_complete, ZX_TIME_INFINITE));

  // Per USB 3.2 section 10.16.2.4, SuperSpeed hubs do not support ClearPortFeature(PORT_ENABLE).
  ASSERT_EQ(fake_device()->clear_port_enable_count(), 0u);
  ASSERT_TRUE(fake_device()->PortEnabled(0));
  ASSERT_TRUE(fake_device()->PortEnabled(1));
  ASSERT_TRUE(fake_device()->GetClearTtBufferCalls().empty());

  // Disconnect both ports before stopping DispatchThread so TearDown's DdkUnbind does not block on
  // state_change_queue_ when removing Port 2, and verify that DeviceRemoved is only invoked for
  // Port 2.
  sync_completion_t disconnect_complete;
  uint32_t removed_ports_mask = 0;
  SetConnectCallback([&](uint32_t port, usb_speed_t speed) {
    ZX_ASSERT(speed == static_cast<usb_speed_t>(-1));
    removed_ports_mask |= (1u << (port - 1));
    sync_completion_signal(&disconnect_complete);
    return ZX_OK;
  });
  DisconnectDevice(0);
  DisconnectDevice(1);
  Interrupt();
  ASSERT_OK(sync_completion_wait(&disconnect_complete, ZX_TIME_INFINITE));
  ASSERT_EQ(removed_ports_mask, 0b10u);
}

}  // namespace
