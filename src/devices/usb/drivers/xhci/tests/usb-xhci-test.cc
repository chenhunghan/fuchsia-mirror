// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/xhci/usb-xhci.h"

#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.hci/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb/cpp/fidl.h>
#include <lib/driver/fake-mmio-reg/cpp/fake-mmio-reg.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fit/defer.h>
#include <lib/sync/completion.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/bti.h>
#include <lib/zx/clock.h>
#include <lib/zx/vmar.h>

#include <atomic>
#include <list>
#include <map>
#include <memory>
#include <thread>

#include <fake-dma-buffer/fake-dma-buffer.h>
#include <gtest/gtest.h>

#include "src/devices/usb/drivers/xhci/tests/test-env.h"
#include "src/lib/testing/predicates/status.h"

namespace usb_xhci {

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

struct FakeTRB;
struct FakePhysAddr {
  uint64_t magic;
  FakeTRB* value;
};

struct FakeTRB : TRB {
  FakeTRB() {
    phys_addr_ = std::make_unique<FakePhysAddr>();
    phys_addr_->magic = kMagicValue;
    phys_addr_->value = this;
  }

  zx_paddr_t phys() { return reinterpret_cast<zx_paddr_t>(phys_addr_.get()); }

  static bool is_valid_paddr(zx_paddr_t phys) {
    return *reinterpret_cast<uint64_t*>(phys) == kMagicValue;
  }

  static FakeTRB* get(zx_paddr_t phys) {
    if (!is_valid_paddr(phys)) {
      return nullptr;
    }
    FakePhysAddr* addr = reinterpret_cast<FakePhysAddr*>(phys);
    return addr->value;
  }

  ~FakeTRB() {
    // Cast to volatile to prevent the compiler
    // from optimizing out this zero operation.
    *reinterpret_cast<volatile uint64_t*>(phys_addr_.get()) = 0;
  }

  static std::unique_ptr<FakeTRB> FromTRB(TRB* trb) {
    return std::unique_ptr<FakeTRB>(static_cast<FakeTRB*>(trb));
  }

  // Magic value to use for determining if a physical address is valid or not.
  // ASAN builds should also trigger an error if we try dereferencing something
  // that isn't valid. This value is a fallback for cases where ASAN isn't being used.
  static constexpr uint64_t kMagicValue = 0x12345678901ABCDEU;
  std::unique_ptr<FakePhysAddr> phys_addr_;
  std::vector<TRB> contig;
  zx_paddr_t next = 0;
  zx_paddr_t prev = 0;
};

class FakeDevice {
 public:
  FakeDevice() {
    constexpr auto kHcsParams2 = 2 * sizeof(uint32_t);
    constexpr auto kHccParams1 = 4 * sizeof(uint32_t);
    constexpr auto kXecp = 320 * sizeof(uint32_t);
    constexpr auto kOffset = 0 * sizeof(uint32_t);
    constexpr auto kHcsParams1 = 1 * sizeof(uint32_t);
    constexpr auto kOffset1 = 5 * sizeof(uint32_t);
    constexpr auto kOffset2 = 6 * sizeof(uint32_t);
    constexpr auto kUsbCmd = 7 * sizeof(uint32_t);
    constexpr auto kUsbSts = 8 * sizeof(uint32_t);
    constexpr auto kUsbPageSize = 9 * sizeof(uint32_t);
    constexpr auto kConfig = 14 * sizeof(uint32_t);
    constexpr auto kCrCr = 13 * sizeof(uint32_t);
    constexpr auto kDcbaa = 19 * sizeof(uint32_t);
    constexpr auto kDoorbellBase = 1024 * sizeof(uint32_t);
    constexpr auto kImodi = 457 * sizeof(uint32_t);

    constexpr uint8_t kCapLength = 0x1c;

    region_.emplace(sizeof(uint32_t), 2048);

    region()[kHcsParams2].SetReadCallback([this] {
      auto params = HCSPARAMS2::Get().FromValue(0);
      params.set_ERST_MAX(4);
      params.set_MAX_SCRATCHPAD_BUFFERS_LOW(max_scratchpad_buffers_low_);
      params.set_MAX_SCRATCHPAD_BUFFERS_HIGH(max_scratchpad_buffers_high_);
      return params.reg_value();
    });

    region()[kHccParams1].SetReadCallback([]() {
      auto params = HCCPARAMS1::Get().FromValue(0);
      params.set_AC64(true);
      params.set_CSZ(true);
      params.set_xECP(320);
      return params.reg_value();
    });

    region()[kXecp].SetReadCallback([=]() {
      auto xecp = (XECP::Get(HCCPARAMS1::Get().FromValue(
                                 static_cast<uint32_t>(region()[4 * sizeof(uint32_t)].Read())))
                       .FromValue(0));
      xecp.set_NEXT(0);
      xecp.set_ID(XECP::UsbLegacySupport);
      if (driver_owned_controller_) {
        xecp.set_reg_value(xecp.reg_value() | 1 << 24);
      } else {
        xecp.set_reg_value(xecp.reg_value() | 1 << 16);
      }
      return xecp.reg_value();
    });

    region()[kXecp].SetWriteCallback([=](uint64_t value) {
      if (value & (1 << 24)) {
        driver_owned_controller_ = true;
      }
    });
    region()[kOffset].SetReadCallback([=]() { return kCapLength; });

    region()[kHcsParams1].SetReadCallback([=]() {
      HCSPARAMS1 parms = HCSPARAMS1::Get().FromValue(0);
      parms.set_MaxIntrs(1);
      parms.set_MaxPorts(4);
      parms.set_MaxSlots(max_slots_);
      return parms.reg_value();
    });

    region()[kOffset1].SetReadCallback([=]() { return 0x1000; });

    region()[kOffset2].SetReadCallback([=]() { return 0x700; });

    region()[kUsbCmd].SetReadCallback([=]() {
      USBCMD cmd = USBCMD::Get(static_cast<uint8_t>(region()[0].Read())).FromValue(0);
      cmd.set_ENABLE(controller_enabled_);
      cmd.set_EWE(event_wrap_enable_);
      cmd.set_HSEE(host_system_error_enable_);
      cmd.set_INTE(irq_enable_);
      cmd.set_RESET(0);
      return cmd.reg_value();
    });

    region()[kUsbCmd].SetWriteCallback([=](uint64_t value) {
      USBCMD cmd = USBCMD::Get(static_cast<uint8_t>(region()[0].Read()))
                       .FromValue(static_cast<uint32_t>(value));
      if (cmd.RESET()) {
        controller_was_reset_ = true;
      }
      controller_enabled_ = cmd.ENABLE();
      event_wrap_enable_ = cmd.EWE();
      host_system_error_enable_ = cmd.HSEE();
      irq_enable_ = cmd.INTE();
    });

    region()[kUsbSts].SetReadCallback([=]() {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(!controller_enabled_);
      return sts.reg_value();
    });

    region()[kUsbPageSize].SetReadCallback([=]() {
      USB_PAGESIZE size = USB_PAGESIZE::Get(kCapLength).FromValue(0);
      size.set_PageSize(1);
      return size.reg_value();
    });

    region()[kConfig].SetReadCallback([=]() {
      CONFIG config = CONFIG::Get(kCapLength).FromValue(0);
      config.set_MaxSlotsEn(slots_enabled_);
      return config.reg_value();
    });

    region()[kConfig].SetWriteCallback([=](uint64_t value) {
      CONFIG config = CONFIG::Get(kCapLength).FromValue(static_cast<uint32_t>(value));
      slots_enabled_ = config.MaxSlotsEn();
    });

    region()[kCrCr].SetWriteCallback([=](uint64_t value) {
      CRCR cr = CRCR::Get(kCapLength).FromValue(value);
      crcr_ = reinterpret_cast<zx_paddr_t>(cr.PTR());
    });

    region()[kDcbaa].SetReadCallback([=]() { return dcbaa_; });
    region()[kDcbaa].SetWriteCallback([=](uint64_t value) {
      auto val = DCBAAP::Get(kCapLength).FromValue(value);
      dcbaa_ = val.PTR();
    });
    doorbell_callback_ = [](uint8_t doorbell, uint8_t target) {};
    for (size_t i = 0; i <= 64; i++) {
      region()[kDoorbellBase + i * sizeof(uint32_t)].SetWriteCallback([=](uint64_t value) {
        auto buffer = mmio();
        auto bell = DOORBELL::Get(DoorbellOffset::Get().ReadFrom(&buffer), 0)
                        .FromValue(static_cast<uint32_t>(value));
        doorbell_callback_(static_cast<uint8_t>(i), static_cast<uint8_t>(bell.Target()));
      });
    }
    region()[kImodi].SetWriteCallback([=](uint64_t value) {
      auto buffer = mmio();
      auto imodi = IMODI::Get(RuntimeRegisterOffset::Get().ReadFrom(&buffer), 0)
                       .FromValue(static_cast<uint32_t>(value));
      imodi_ = static_cast<uint16_t>(imodi.MODI());
    });
    for (uint8_t port = 1; port <= kMaxPorts; port++) {
      region()[PORTSC::Get(kCapLength, port).addr()].SetReadCallback([this, port]() {
        return PORTSC::Get(kCapLength, port)
            .FromValue(0)
            .set_PortSpeed(port_speeds_[port - 1])
            .reg_value();
      });
    }

    // Control register
  }

  fdf::MmioBuffer mmio() { return region_->GetMmioBuffer(); }
  fake_mmio::FakeMmioRegRegion& region() { return *region_; }

  void set_irq_signaller(zx::unowned_interrupt signaller) { irq_signaller_ = std::move(signaller); }

  void SetDoorbellCallback(fit::function<void(uint8_t, uint8_t)> callback) {
    doorbell_callback_ = std::move(callback);
  }

  void set_max_slots(uint8_t max_slots) { max_slots_ = max_slots; }

  zx_paddr_t dcbaa_phys() const { return dcbaa_; }

  FakeTRB* crcr() { return FakeTRB::get(crcr_); }

  void set_max_scratchpad_buffers(uint8_t low, uint8_t high) {
    max_scratchpad_buffers_low_ = low;
    max_scratchpad_buffers_high_ = high;
  }

  void set_port_speed(uint8_t port, usb_speed_t speed) {
    if (port >= 1 && port <= kMaxPorts) {
      port_speeds_[port - 1] = speed;
    }
  }

 private:
  static constexpr uint8_t kMaxPorts = 4;
  std::optional<fake_mmio::FakeMmioRegRegion> region_;
  zx::unowned_interrupt irq_signaller_;
  bool driver_owned_controller_ = false;
  bool controller_enabled_ = false;
  bool controller_was_reset_ = false;
  bool event_wrap_enable_ = false;
  bool irq_enable_ = false;
  bool host_system_error_enable_ = false;
  uint8_t max_slots_ = 32;
  uint8_t max_scratchpad_buffers_low_ = 1;
  uint8_t max_scratchpad_buffers_high_ = 0;
  usb_speed_t port_speeds_[kMaxPorts] = {};
  uint32_t slots_enabled_ = 0;
  zx_paddr_t crcr_ = 0;
  zx_paddr_t dcbaa_ = 0;
  uint16_t imodi_;
  fit::function<void(uint8_t, uint8_t)> doorbell_callback_;
};

struct FakeUsbDevice {
  uint32_t device_id;
  uint32_t hub_id;
  usb_speed_t speed;
  bool fake_root_hub;
};

class FakeUsbBus : public ddk::UsbBusInterfaceProtocol<FakeUsbBus> {
 public:
  FakeUsbBus() = default;

  void reset() { sync_completion_reset(&completion_); }

  void wait() { sync_completion_wait(&completion_, ZX_TIME_INFINITE); }

  void reset_remove() { sync_completion_reset(&remove_completion_); }

  void wait_for_remove() { sync_completion_wait(&remove_completion_, ZX_TIME_INFINITE); }

  usb_bus_interface_protocol_t* proto() {
    proto_.ctx = this;
    proto_.ops = &usb_bus_interface_protocol_ops_;
    return &proto_;
  }

  void set_max_slots(uint32_t max_slots) { max_slots_ = max_slots; }
  using AddDeviceHook = fit::function<std::optional<zx_status_t>(uint32_t, uint32_t, usb_speed_t)>;
  void set_add_device_hook(AddDeviceHook hook) {
    auto shared = std::make_shared<AddDeviceHook>(std::move(hook));
    fbl::AutoLock _(&hook_lock_);
    add_device_hook_ = std::move(shared);
  }
  void clear_add_device_hook() {
    std::shared_ptr<AddDeviceHook> dropped;
    {
      fbl::AutoLock _(&hook_lock_);
      dropped = std::move(add_device_hook_);
    }
  }

  zx_status_t UsbBusInterfaceAddDevice(uint32_t device_id, uint32_t hub_id, usb_speed_t speed) {
    std::shared_ptr<AddDeviceHook> hook;
    {
      fbl::AutoLock _(&hook_lock_);
      hook = add_device_hook_;
    }
    if (hook && *hook) {
      if (std::optional<zx_status_t> status = (*hook)(device_id, hub_id, speed);
          status.has_value()) {
        sync_completion_signal(&completion_);
        return *status;
      }
    }
    FakeUsbDevice fake_device;
    fake_device.device_id = device_id;
    fake_device.hub_id = hub_id;
    fake_device.speed = speed;
    fake_device.fake_root_hub = device_id >= max_slots_;
    {
      fbl::AutoLock _(&devices_lock_);
      devices_[device_id] = fake_device;
    }
    sync_completion_signal(&completion_);
    return ZX_OK;
  }

  void set_strict_remove(bool strict) { strict_remove_.store(strict, std::memory_order_release); }
  using RemoveDeviceHook = fit::function<std::optional<zx_status_t>(uint32_t)>;
  void set_remove_device_hook(RemoveDeviceHook hook) {
    auto shared = std::make_shared<RemoveDeviceHook>(std::move(hook));
    fbl::AutoLock _(&hook_lock_);
    remove_device_hook_ = std::move(shared);
  }
  void clear_remove_device_hook() {
    std::shared_ptr<RemoveDeviceHook> dropped;
    {
      fbl::AutoLock _(&hook_lock_);
      dropped = std::move(remove_device_hook_);
    }
  }
  size_t remove_device_calls() const {
    return remove_device_calls_.load(std::memory_order_acquire);
  }
  void reset_remove_device_calls() { remove_device_calls_.store(0, std::memory_order_release); }

  zx_status_t UsbBusInterfaceRemoveDevice(uint32_t device_id) {
    remove_device_calls_.fetch_add(1, std::memory_order_acq_rel);
    std::shared_ptr<RemoveDeviceHook> hook;
    {
      fbl::AutoLock _(&hook_lock_);
      hook = remove_device_hook_;
    }
    bool erased = false;
    {
      fbl::AutoLock _(&devices_lock_);
      erased = devices_.erase(device_id) > 0;
    }
    sync_completion_signal(&remove_completion_);
    if (hook && *hook) {
      if (std::optional<zx_status_t> status = (*hook)(device_id); status.has_value()) {
        return *status;
      }
    }
    if (strict_remove_.load(std::memory_order_acquire) && !erased) {
      return ZX_ERR_BAD_STATE;
    }
    return ZX_OK;
  }

  zx_status_t UsbBusInterfaceResetPort(uint32_t hub_id, uint32_t port, bool enumerating) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  zx_status_t UsbBusInterfaceReinitializeDevice(uint32_t device_id) { return ZX_ERR_NOT_SUPPORTED; }
  std::map<uint32_t, FakeUsbDevice> devices() const {
    fbl::AutoLock _(&devices_lock_);
    return devices_;
  }

 private:
  uint32_t max_slots_ = 32;
  std::atomic<bool> strict_remove_{false};
  std::atomic<size_t> remove_device_calls_{0};
  fbl::Mutex hook_lock_;
  std::shared_ptr<AddDeviceHook> add_device_hook_ __TA_GUARDED(hook_lock_);
  std::shared_ptr<RemoveDeviceHook> remove_device_hook_ __TA_GUARDED(hook_lock_);
  mutable fbl::Mutex devices_lock_;
  std::map<uint32_t, FakeUsbDevice> devices_ __TA_GUARDED(devices_lock_);
  sync_completion_t completion_;
  sync_completion_t remove_completion_;
  usb_bus_interface_protocol_t proto_;
};

class XhciTestEnvironment : fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    device_server_.Initialize(component::kDefaultInstance);
    EXPECT_OK(
        device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs));

    fdf_fake::FakePDev::Config config;
    config.mmios[0] = fake_device_.mmio();
    config.irqs[0] = {};
    EXPECT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &config.irqs[0]));
    fake_device_.set_irq_signaller(config.irqs[0].borrow());
    config.use_fake_bti = true;
    pdev_server_.SetConfig(std::move(config));
    return to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_server_.GetInstanceHandler(fdf::Dispatcher::GetCurrent()->async_dispatcher()), "pdev");
  }

  FakeDevice& fake_device() { return fake_device_; }

 private:
  compat::DeviceServer device_server_;
  fdf_fake::FakePDev pdev_server_;
  FakeDevice fake_device_;
};

class XhciTestConfig final {
 public:
  using DriverType = usb_xhci::UsbXhci;
  using EnvironmentType = XhciTestEnvironment;
};

using TestRequest = usb::CallbackRequest<sizeof(max_align_t)>;
class XhciHarness : public ::testing::Test {
 public:
  FakeTRB* CreateTRB() {
    auto it = trbs_.insert(trbs_.end(), std::make_unique<FakeTRB>());
    (*it)->control = 0;
    (*it)->ptr = 0;
    (*it)->status = 0;
    return it->get();
  }

  FakeTRB* CreateTRBs(size_t count) {
    auto it = trbs_.insert(trbs_.end(), std::make_unique<FakeTRB>());
    (*it)->control = 0;
    (*it)->ptr = 0;
    (*it)->status = 0;
    (*it)->contig.resize(count);
    return it->get();
  }

  size_t GetMaxDeviceCount() { return driver_test().driver()->UsbHciGetMaxDeviceCount(); }

  void RequestQueue(TestRequest request) {
    static const usb_protocol_ops_t ops = {
        .request_queue =
            [](void* ctx, usb_request_t* usb_request,
               const usb_request_complete_callback_t* complete_cb) {
              auto* driver = static_cast<UsbXhci*>(ctx);
              driver->UsbHciRequestQueue(usb_request, complete_cb);
            },
    };
    usb_protocol_t proto = {
        .ops = &ops,
        .ctx = driver_test().driver(),
    };
    ddk::UsbProtocolClient client(&proto);
    TestRequest::Queue(std::move(request), client);
  }

  template <typename Callback>
  zx_status_t AllocateRequest(std::optional<TestRequest>* request, uint32_t device_id,
                              uint64_t data_size, uint8_t endpoint, Callback callback) {
    zx_status_t result =
        TestRequest::Alloc(request, data_size, endpoint,
                           driver_test().driver()->UsbHciGetRequestSize(), std::move(callback));
    if (result != ZX_OK || !request->has_value()) {
      return result != ZX_OK ? result : ZX_ERR_INTERNAL;
    }
    TestRequest& allocated = request->value();
    void* virt;
    allocated.Mmap(&virt);
    static_assert(sizeof(uint64_t) == sizeof(void*));
    const size_t page_size = zx_system_get_page_size();
    size_t phys_count = fbl::round_up(data_size, page_size) / page_size;
    // Need to use malloc for compatibility with the C ABI (which will eventually call free)
    zx_paddr_t* phys_list = static_cast<zx_paddr_t*>(malloc(sizeof(zx_paddr_t) * phys_count));
    if (phys_count > 0 && phys_list == nullptr) {
      return ZX_ERR_NO_MEMORY;
    }
    for (size_t i = 0; i < phys_count; i++) {
      auto trb = CreateTRB();
      // Keep consecutive fake physical addresses non-contiguous so phys_iter does not
      // coalesce distinct pages if two FakePhysAddr heap allocations land one page apart.
      while (i > 0 && trb->phys() == phys_list[i - 1] + page_size) {
        trb = CreateTRB();
      }
      trb->ptr = reinterpret_cast<uint64_t>(virt) + (page_size * i);
      phys_list[i] = trb->phys();
    }
    allocated.request()->phys_count = phys_count;
    allocated.request()->phys_list = phys_list;
    allocated.request()->header.device_id = device_id;
    return ZX_OK;
  }

  uint8_t AllocateSlot() {
    fbl::AutoLock _(&slot_lock_);
    if (slot_freelist_.empty()) {
      slot_id_++;
      return slot_id_;
    }
    uint8_t retval = slot_freelist_.back();
    slot_freelist_.pop_back();
    return retval;
  }

  void FreeSlot(uint8_t slot) {
    fbl::AutoLock _(&slot_lock_);
    slot_freelist_.push_back(slot);
  }

  FakeUsbDevice ConnectDevice(uint8_t port, usb_speed_t speed) {
    std::optional<HubInfo> hub;
    uint8_t slot = AllocateSlot();
    driver_test().RunInEnvironmentTypeContext(
        [port, speed](XhciTestEnvironment& env) { env.fake_device().set_port_speed(port, speed); });
    driver_test().driver()->GetPortState(port).is_connected = true;
    driver_test().driver()->GetPortState(port).link_active = true;
    driver_test().driver()->GetPortState(port).slot_id = slot;
    driver_test().driver()->SetDeviceInformation(slot, port, hub);
    driver_test().driver()->AddressDeviceCommand(slot, port, hub, true);
    bus_.reset();
    driver_test().driver()->DeviceOnline(slot, port, speed);
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this]() { bus_.wait(); }));
    return bus_.devices().at(slot - 1);
  }

  // Runs DeviceOffline(slot) to completion on interrupter 0's executor. FakeUsbBus::wait_for_remove
  // signals inside UsbBusInterfaceRemoveDevice on ddk_interaction_dispatcher_ before PostCallback
  // invokes bridge.completer.complete_ok(), so pumping RunUntilIdle() until the outer DeviceOffline
  // promise resolves ensures any chained continuations (such as DisableSlotCommand) have finished.
  void RunDeviceOffline(uint32_t slot) {
    bus_.reset_remove();
    auto offline_completed = std::make_shared<libsync::Completion>();
    UsbXhci* xhci = driver_test().driver();
    xhci->ScheduleTask(0,
                       xhci->DeviceOffline(slot)
                           .then([offline_completed](fpromise::result<void, zx_status_t>& result) {
                             offline_completed->Signal();
                             return result;
                           })
                           .box());
    xhci->RunUntilIdle();
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait_for_remove(); }));
    const zx::time deadline = zx::deadline_after(zx::sec(30));
    while (!offline_completed->signaled()) {
      ASSERT_LT(zx::clock::get_monotonic(), deadline)
          << "Timed out waiting for DeviceOffline(" << slot << ") to complete";
      xhci->RunUntilIdle();
      if (!offline_completed->signaled()) {
        zx::nanosleep(zx::deadline_after(zx::usec(50)));
      }
    }
  }

  void EnableEndpoint(uint32_t device_id, uint8_t ep_num, bool is_in_endpoint) {
    usb_endpoint_descriptor_t ep_desc = {};
    ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk);
    ep_desc.b_endpoint_address = ep_num | (is_in_endpoint ? 0x80 : 0);
    driver_test().driver()->UsbHciEnableEndpoint(device_id, &ep_desc, nullptr);
  }

  zx_status_t CompleteCommand(TRB* trb, CommandCompletionEvent* event) {
    std::unique_ptr<TRBContext> context;
    zx_status_t status = driver_test().driver()->GetCommandRing()->CompleteTRB(trb, &context);
    if (status != ZX_OK) {
      return status;
    }
    context->completer->complete_ok(event);
    return ZX_OK;
  }

  void SetDoorbellListener(fit::function<void(uint8_t, uint8_t)> listener) {
    driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
      env.fake_device().SetDoorbellCallback(std::move(listener));
    });
  }

  void SetEnumerateDeviceListener(
      fit::function<void(uint8_t, const std::optional<HubInfo>&)> listener) {
    enumerate_device_listener_ = std::move(listener);
  }

  void OnEnumerateDevice(uint8_t port, const std::optional<HubInfo>& hub_info) {
    if (enumerate_device_listener_) {
      enumerate_device_listener_(port, hub_info);
    }
  }

  FakeTRB* crcr_next() {
    return FakeTRB::get(driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
        [](XhciTestEnvironment& env) { return env.fake_device().crcr()->next; }));
  }

  ~XhciHarness() { trbs_.clear(); }

  fdf_testing::ForegroundDriverTest<XhciTestConfig>& driver_test() { return driver_test_; }

 protected:
  FakeUsbBus bus_;

 private:
  fdf_testing::ForegroundDriverTest<XhciTestConfig> driver_test_;
  fbl::Mutex slot_lock_;
  std::vector<uint8_t> slot_freelist_ __TA_GUARDED(slot_lock_);
  uint8_t slot_id_ __TA_GUARDED(slot_lock_) = 0;
  std::list<std::unique_ptr<FakeTRB>> trbs_;
  fit::function<void(uint8_t, const std::optional<HubInfo>&)> enumerate_device_listener_;
};

class XhciMmioHarness : public XhciHarness {
 public:
  void SetUp() override {
    ASSERT_TRUE(driver_test()
                    .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                      xhci_config::Config fake_config;
                      fake_config.enable_suspend() = false;
                      args.config(fake_config.ToVmo());
                    })
                    .is_ok());
    ASSERT_OK(driver_test().driver()->TestInit(this));
    driver_test().driver()->UsbHciSetBusInterface(bus_.proto());
  }

  void TearDown() override {
    if (!stopped_) {
      ASSERT_TRUE(driver_test().StopDriver().is_ok());
    }
  }

 protected:
  struct TestHub {
    uint32_t id = 0;
    uint8_t slot = 0;
    fbl::RefPtr<DeviceState> state;
  };

  struct OfflineResult {
    std::atomic<bool> done{false};
    std::atomic<bool> is_error{false};
    std::atomic<zx_status_t> status{ZX_ERR_INTERNAL};
    libsync::Completion completion;
  };

  template <typename Predicate>
  static bool WaitUntil(Predicate&& predicate, zx::duration timeout = zx::sec(5)) {
    const zx::time deadline = zx::deadline_after(timeout);
    while (!predicate() && zx::clock::get_monotonic() < deadline) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    return predicate();
  }

  fit::deferred_callback InstallCommandDoorbellListener(
      std::shared_ptr<std::atomic<uint8_t>> fail_disable_slot_id = nullptr) {
    UsbXhci* xhci = driver_test().driver();
    auto success_event = std::make_shared<CommandCompletionEvent>();
    success_event->set_CompletionCode(CommandCompletionEvent::Success);
    auto error_event = std::make_shared<CommandCompletionEvent>();
    error_event->set_CompletionCode(CommandCompletionEvent::ContextStateError);
    SetDoorbellListener([this, xhci, success_event, error_event,
                         fail_disable_slot_id = std::move(fail_disable_slot_id)](
                            uint8_t doorbell, uint8_t /*target*/) {
      if (doorbell != 0) {
        return;
      }
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending) {
        CommandCompletionEvent* event = success_event.get();
        if (cmd.trb && Control::FromTRB(cmd.trb).Type() == Control::DisableSlot) {
          DisableSlot disable_cmd;
          static_cast<TRB&>(disable_cmd) = *cmd.trb;
          const uint8_t slot = static_cast<uint8_t>(disable_cmd.slot());
          if (fail_disable_slot_id &&
              fail_disable_slot_id->load(std::memory_order_acquire) == slot) {
            event = error_event.get();
          } else if (slot >= 1 && slot <= 32) {
            FreeSlot(slot);
          }
        }
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(event);
        }
      }
    });
    return fit::defer_callback([this, xhci] {
      xhci->SetHubRemoveEnqueuedCompletionForTesting(nullptr);
      bus_.clear_add_device_hook();
      bus_.clear_remove_device_hook();
      bus_.set_strict_remove(false);
      if (!stopped_ && xhci->FlushDdkCallbacksForTesting() != ZX_OK) {
        stopped_ = true;
        (void)driver_test().StopDriver();
      }
      SetDoorbellListener([](uint8_t, uint8_t) {});
    });
  }

  class AddDeviceBlocker {
   public:
    AddDeviceBlocker(FakeUsbBus* bus, uint32_t target_device_id,
                     std::optional<zx_status_t> return_status)
        : bus_(bus),
          entered_(std::make_shared<libsync::Completion>()),
          release_(std::make_shared<libsync::Completion>()) {
      bus_->set_add_device_hook(
          [target_device_id, return_status, entered = entered_, release = release_](
              uint32_t device_id, uint32_t /*hub_id*/,
              usb_speed_t /*speed*/) -> std::optional<zx_status_t> {
            if (device_id == target_device_id) {
              entered->Signal();
              EXPECT_OK(release->Wait(zx::sec(5)));
              return return_status;
            }
            return std::nullopt;
          });
    }

    ~AddDeviceBlocker() {
      release_->Signal();
      bus_->clear_add_device_hook();
    }

    zx_status_t WaitEntered(zx::duration timeout = zx::sec(5)) { return entered_->Wait(timeout); }
    void Release() { release_->Signal(); }
    const std::shared_ptr<libsync::Completion>& release_completion() const { return release_; }

   private:
    FakeUsbBus* bus_;
    std::shared_ptr<libsync::Completion> entered_;
    std::shared_ptr<libsync::Completion> release_;
  };

  class HubRemoveOp {
   public:
    HubRemoveOp(UsbXhci* xhci, uint32_t hub_id, uint32_t port,
                std::shared_ptr<libsync::Completion> release_on_cleanup)
        : enqueued_(std::make_shared<libsync::Completion>()),
          done_(std::make_shared<libsync::Completion>()),
          status_(std::make_shared<std::atomic<zx_status_t>>(ZX_ERR_INTERNAL)),
          release_on_cleanup_(std::move(release_on_cleanup)) {
      xhci->SetHubRemoveEnqueuedCompletionForTesting(enqueued_);
      thread_ = std::thread([xhci, hub_id, port, status = status_, done = done_] {
        status->store(xhci->UsbHciHubDeviceRemoved(hub_id, port), std::memory_order_release);
        done->Signal();
      });
    }

    ~HubRemoveOp() {
      if (release_on_cleanup_) {
        release_on_cleanup_->Signal();
      }
      if (thread_.joinable()) {
        thread_.join();
      }
    }

    zx_status_t WaitEnqueued(zx::duration timeout = zx::sec(5)) { return enqueued_->Wait(timeout); }
    bool IsDone() const { return done_->signaled(); }
    zx_status_t WaitAndJoin(zx::duration timeout = zx::sec(5)) {
      zx_status_t status = done_->Wait(timeout);
      if (status == ZX_OK && thread_.joinable()) {
        thread_.join();
      }
      return status;
    }
    zx_status_t status() const { return status_->load(std::memory_order_acquire); }

   private:
    std::shared_ptr<libsync::Completion> enqueued_;
    std::shared_ptr<libsync::Completion> done_;
    std::shared_ptr<std::atomic<zx_status_t>> status_;
    std::shared_ptr<libsync::Completion> release_on_cleanup_;
    std::thread thread_;
  };

  TestHub ConnectConfiguredHub(uint8_t root_port, uint8_t num_ports = 4, bool multi_tt = true) {
    UsbXhci* xhci = driver_test().driver();
    FakeUsbDevice hub = ConnectDevice(root_port, USB_SPEED_HIGH);
    TestHub result;
    result.id = hub.device_id;
    result.slot = static_cast<uint8_t>(hub.device_id + 1u);
    result.state = xhci->GetDeviceState(hub.device_id);
    EXPECT_TRUE(result.state);
    usb_hub_descriptor_t hub_desc = {};
    hub_desc.b_nbr_ports = num_ports;
    EXPECT_OK(xhci->UsbHciConfigureHub(result.id, USB_SPEED_HIGH, &hub_desc, multi_tt));
    return result;
  }

  uint8_t AddressRootSlot(uint8_t port) {
    UsbXhci* xhci = driver_test().driver();
    const uint8_t slot = AllocateSlot();
    xhci->SetDeviceInformation(slot, port, std::nullopt);
    xhci->ScheduleTask(0, xhci->AddressDeviceCommand(slot, port, std::nullopt, true));
    xhci->RunUntilIdle();
    return slot;
  }

  uint8_t AttachHubChild(const fbl::RefPtr<DeviceState>& hub_state, uint8_t parent_port,
                         uint8_t rh_port, bool bring_online = false,
                         usb_speed_t speed = USB_SPEED_FULL) {
    UsbXhci* xhci = driver_test().driver();
    const uint8_t child_slot = AllocateSlot();
    HubInfo info = {};
    info.hub_state = hub_state;
    info.speed = speed;
    info.hub_speed = USB_SPEED_HIGH;
    info.rh_port = rh_port;
    info.parent_port_number = parent_port;
    xhci->SetDeviceInformation(child_slot, parent_port, info);
    xhci->ScheduleTask(0, xhci->AddressDeviceCommand(child_slot, parent_port, info, true));
    xhci->RunUntilIdle();
    if (bring_online) {
      bus_.reset();
      EXPECT_OK(xhci->DeviceOnline(child_slot, parent_port, speed));
      EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait(); }));
    }
    return child_slot;
  }

  std::shared_ptr<OfflineResult> ScheduleDeviceOffline(uint8_t slot) {
    UsbXhci* xhci = driver_test().driver();
    auto result = std::make_shared<OfflineResult>();
    xhci->ScheduleTask(0, xhci->DeviceOffline(slot)
                              .then([result](fpromise::result<void, zx_status_t>& r)
                                        -> fpromise::result<void, zx_status_t> {
                                result->is_error.store(r.is_error(), std::memory_order_release);
                                result->status.store(r.is_ok() ? ZX_OK : r.error(),
                                                     std::memory_order_release);
                                result->done.store(true, std::memory_order_release);
                                result->completion.Signal();
                                return fpromise::ok();
                              })
                              .box());
    return result;
  }

  bool stopped_ = false;
};

fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> TransferRing::TakePendingTRBs() {
  fbl::AutoLock al(&mutex_);
  return std::move(pending_trbs_);
}

zx::result<> UsbXhci::TestInit(void* test_harness) {
  test_harness_ = test_harness;
  return Init(ddk_fake::CreateBufferFactory());
}

void EventRing::ScheduleTask(fpromise::promise<void, zx_status_t> promise) {
  SchedulePromiseWithXhciExecutorPolicy(executor_, hci_, std::move(promise));
}

void EventRing::RunUntilIdle() { executor_.run_until_idle(); }

void EventRing::ClearPendingPortStatusChanges() { executor_.run_until_idle(); }

zx_status_t TransferRing::AllocateTRB(TRB** trb, State* state) {
  fbl::AutoLock _(&mutex_);
  if (state) {
    state->pcs = pcs_;
    state->trbs = trbs_;
  }
  auto new_trb = hci_->GetTestHarness<XhciHarness>()->CreateTRB();
  new_trb->prev = static_cast<FakeTRB*>(trbs_)->phys();
  static_cast<FakeTRB*>(trbs_)->next = new_trb->phys();
  trbs_ = new_trb;
  trbs_->ptr = 0;
  trbs_->status = pcs_;
  *trb = trbs_;
  return ZX_OK;
}

zx::result<ContiguousTRBInfo> TransferRing::AllocateContiguous(size_t count) {
  fbl::AutoLock _(&mutex_);
  auto new_trb = hci_->GetTestHarness<XhciHarness>()->CreateTRBs(count);
  new_trb->prev = static_cast<FakeTRB*>(trbs_)->phys();
  static_cast<FakeTRB*>(trbs_)->next = new_trb->phys();
  trbs_ = new_trb->contig.data();
  trbs_->ptr = 0;
  trbs_->status = pcs_;
  ContiguousTRBInfo info;
  info.trbs = cpp20::span(trbs_, count);
  return zx::ok(info);
}

constexpr auto kPeekPtr = 0x13823990000;

zx::result<CRCR> TransferRing::TransferRing::PeekCommandRingControlRegister(uint8_t cap_length) {
  fbl::AutoLock l(&mutex_);
  CRCR cr;
  cr.set_RCS(pcs_);
  cr.set_PTR(kPeekPtr);
  return zx::ok(cr);
}

zx_status_t TransferRing::CompleteTRB(TRB* trb, std::unique_ptr<TRBContext>* context) {
  fbl::AutoLock _(&mutex_);
  if (pending_trbs_.is_empty()) {
    return ZX_ERR_CANCELED;
  }
  dequeue_trb_ = trb;
  *context = pending_trbs_.pop_front();
  if (trb != (*context)->trb) {
    return ZX_ERR_IO;
  }
  return ZX_OK;
}

void TransferRing::CommitTransaction(const State& start) {}

zx_status_t TransferRing::AssignContext(TRB* trb, std::unique_ptr<TRBContext> context, TRB* first,
                                        TRB* setup) {
  fbl::AutoLock _(&mutex_);
  if (context->token != token_) {
    return ZX_ERR_INVALID_ARGS;
  }
  context->trb = trb;
  pending_trbs_.push_back(std::move(context));
  return ZX_OK;
}

zx_status_t TransferRing::Init(size_t page_size, const zx::bti& bti, EventRing* ring, bool is_32bit,
                               fdf::MmioBuffer* mmio, UsbXhci* hci) {
  fbl::AutoLock _(&mutex_);
  if (trbs_ != nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  page_size_ = page_size;
  bti_ = &bti;
  ring_ = ring;
  is_32_bit_ = is_32bit;
  mmio_ = mmio;
  isochronous_ = false;
  token_++;
  stalled_ = false;
  hci_ = hci;
  trbs_ = hci_->GetTestHarness<XhciHarness>()->CreateTRB();
  static_assert(sizeof(uint64_t) == sizeof(this));
  trbs_->ptr = reinterpret_cast<uint64_t>(this);
  trbs_->status = pcs_;
  trb_start_phys_ = static_cast<FakeTRB*>(trbs_)->phys();
  return ZX_OK;
}

CRCR TransferRing::TransferRing::phys(uint8_t cap_length) __TA_NO_THREAD_SAFETY_ANALYSIS {
  CRCR cr = CRCR::Get(cap_length).FromValue(trb_start_phys_);
  assert(trb_start_phys_);
  cr.set_RCS(pcs_);
  return cr;
}

TransferRing::State TransferRing::SaveState() {
  fbl::AutoLock _(&mutex_);
  State state;
  state.pcs = pcs_;
  state.trbs = trbs_;
  return state;
}

void TransferRing::Restore(const State& state) {
  fbl::AutoLock _(&mutex_);
  trbs_ = state.trbs;
  pcs_ = state.pcs;
}

zx_status_t TransferRing::AddTRB(const TRB& trb, std::unique_ptr<TRBContext> context) {
  fbl::AutoLock _(&mutex_);
  if (context->token != token_) {
    return ZX_ERR_INVALID_ARGS;
  }
  FakeTRB* alloc_trb;
  alloc_trb = hci_->GetTestHarness<XhciHarness>()->CreateTRB();
  alloc_trb->prev = static_cast<FakeTRB*>(trbs_)->phys();
  static_cast<FakeTRB*>(trbs_)->next = alloc_trb->phys();
  trbs_ = alloc_trb;
  alloc_trb->control = trb.control;
  alloc_trb->ptr = trb.ptr;
  alloc_trb->status = trb.status;
  context->token = token_;
  context->trb = alloc_trb;
  pending_trbs_.push_back(std::move(context));
  return ZX_OK;
}

zx_status_t TransferRing::Deinit() {
  fbl::AutoLock _(&mutex_);
  if (!trbs_) {
    return ZX_ERR_BAD_STATE;
  }
  trbs_ = nullptr;
  dequeue_trb_ = nullptr;
  pcs_ = true;
  return ZX_OK;
}

zx_status_t TransferRing::DeinitIfActive() __TA_NO_THREAD_SAFETY_ANALYSIS {
  if (trbs_) {
    return Deinit();
  }
  return ZX_OK;
}

zx_paddr_t TransferRing::VirtToPhys(TRB* trb) {
  auto phys = static_cast<FakeTRB*>(trb)->phys();
  ZX_ASSERT(FakeTRB::is_valid_paddr(phys));
  return phys;
}

zx_status_t EventRingSegmentTable::Init(size_t page_size, const zx::bti& bti, bool is_32bit,
                                        uint32_t erst_max, ERSTSZ erst_size,
                                        const dma_buffer::BufferFactory& factory,
                                        fdf::MmioBuffer* mmio) {
  erst_size_ = erst_size;
  bti_ = &bti;
  page_size_ = page_size;
  is_32bit_ = is_32bit;
  mmio_.emplace(mmio->View(0));
  zx_status_t status = factory.CreatePaged(bti, zx_system_get_page_size(),
                                           dma_buffer::CacheOptions::kDisabled, &erst_);
  if (status != ZX_OK) {
    return status;
  }

  count_ = page_size / sizeof(ERSTEntry);
  if (count_ > erst_max) {
    count_ = erst_max;
  }
  entries_ = static_cast<ERSTEntry*>(erst_->virt());
  return ZX_OK;
}

zx_status_t EventRing::Init(size_t page_size, const zx::bti& bti, fdf::MmioBuffer* buffer,
                            bool is_32bit, uint32_t erst_max, ERSTSZ erst_size, ERDP erdp_reg,
                            IMAN iman_reg, uint8_t cap_length, HCSPARAMS1 hcs_params_1,
                            CommandRing* command_ring, DoorbellOffset doorbell_offset, UsbXhci* hci,
                            HCCPARAMS1 hcc_params_1, uint64_t* dcbaa, uint16_t interrupter,
                            inspect::Node* interrupter_node) {
  fbl::AutoLock _(&segment_mutex_);
  erdp_reg_ = erdp_reg;
  hcs_params_1_ = hcs_params_1;
  mmio_ = buffer;
  bti_ = &bti;
  page_size_ = page_size;
  is_32bit_ = is_32bit;
  mmio_ = buffer;
  iman_reg_ = iman_reg;
  cap_length_ = cap_length;
  command_ring_ = command_ring;
  doorbell_offset_ = doorbell_offset;
  hci_ = hci;
  hcc_params_1_ = hcc_params_1;
  dcbaa_ = dcbaa;
  static_assert(sizeof(zx_paddr_t) == sizeof(this));
  erdp_phys_ = reinterpret_cast<zx_paddr_t>(this);
  interrupter_ = interrupter;
  return segments_.Init(page_size, bti, is_32bit, erst_max, erst_size, hci->buffer_factory(),
                        mmio_);
}

size_t EventRing::GetPressure() { return 0; }

zx_status_t Interrupter::Init(uint16_t interrupter, size_t page_size, fdf::MmioBuffer* buffer,
                              const RuntimeRegisterOffset& offset, uint32_t erst_max,
                              DoorbellOffset doorbell_offset, UsbXhci* hci, HCCPARAMS1 hcc_params_1,
                              uint64_t* dcbaa) {
  interrupter_ = interrupter;
  hci_ = hci;
  return ZX_OK;
}

zx_status_t Interrupter::Start(const RuntimeRegisterOffset& offset,
                               fdf::MmioView interrupter_regs) {
  active_ = true;
  return ZX_OK;
}

zx_status_t Interrupter::StartIrqThread() { return ZX_OK; }

fpromise::promise<void, zx_status_t> Interrupter::Timeout(zx::time deadline) {
  return fpromise::make_result_promise<void, zx_status_t>(fpromise::ok());
}

// Enumerates a device as specified in xHCI section 4.3 starting from step 4
// This method should be called once the physical port of a device has been
// initialized.
fpromise::promise<void, zx_status_t> EnumerateDevice(UsbXhci* hci, uint8_t port,
                                                     std::optional<HubInfo> hub_info) {
  if (auto* harness = hci->GetTestHarness<XhciHarness>()) {
    harness->OnEnumerateDevice(port, hub_info);
  }
  return hci->EnableSlotCommand().discard_value();
}

struct alignas(4096) FakeVMO {
  size_t size;
  uint32_t alignment_log2;
  bool enable_cache;
  zx::vmo backing_storage;
  void* virt;
};

TEST_F(XhciMmioHarness, QueueControlRequest) {
  ConnectDevice(1, USB_SPEED_HIGH);
  bool rang = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 1 && target == 1) {
      rang = true;
    }
  });

  std::optional<TestRequest> request;
  bool invoked = false;
  AllocateRequest(&request, 0, zx_system_get_page_size() * 2, 0, [&](TestRequest request) {
    invoked = true;
    void** parameters;
    request.Mmap(reinterpret_cast<void**>(&parameters));
    EXPECT_EQ(*parameters, parameters);
  });
  request->request()->setup.bm_request_type = kStandardDeviceIn;
  request->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  request->request()->setup.w_value = usb_descriptor_w_value(fdescriptor::DescriptorType::kDevice);
  ASSERT_LE(zx_system_get_page_size() * 2, static_cast<uint32_t>(UINT16_MAX));
  request->request()->setup.w_length = static_cast<uint16_t>(zx_system_get_page_size() * 2);
  RequestQueue(std::move(*request));
  ASSERT_TRUE(rang);
  // Find slot context pointer in address device command
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  ASSERT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  auto control = static_cast<unsigned char*>(reinterpret_cast<FakeVMO*>(cr->ptr)->virt);
  auto endpoint_context = reinterpret_cast<EndpointContext*>(control + (64 * 2));
  auto ring_phys =
      static_cast<zx_paddr_t>(static_cast<uint64_t>(endpoint_context->dequeue_pointer_a) |
                              (static_cast<uint64_t>(endpoint_context->dequeue_pointer_b) << 32)) &
      (~1);
  auto trb = FakeTRB::get(ring_phys);
  auto initial_trb = trb;
  // Setup
  trb = FakeTRB::get(trb->next);
  auto setup_trb = static_cast<struct Setup*>(static_cast<TRB*>(trb));
  ASSERT_EQ(setup_trb->length(), 8UL);
  ASSERT_EQ(setup_trb->IDT(), 1UL);
  ASSERT_EQ(setup_trb->TRT(), Setup::IN);
  // Data
  trb = FakeTRB::get(trb->next);
  auto data_trb = static_cast<ControlData*>(static_cast<TRB*>(trb));
  ASSERT_EQ(data_trb->DIRECTION(), 1UL);
  ASSERT_EQ(data_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(data_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(data_trb->SIZE(), 1UL);
  ASSERT_TRUE(data_trb->ISP());
  ASSERT_TRUE(data_trb->NO_SNOOP());
  void** virt = reinterpret_cast<void**>(FakeTRB::get(static_cast<zx_paddr_t>(data_trb->ptr))->ptr);
  *virt = virt;
  // Normal
  trb = FakeTRB::get(trb->next);
  auto normal_trb = static_cast<Normal*>(static_cast<TRB*>(trb));
  ASSERT_EQ(normal_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(normal_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(normal_trb->SIZE(), 0UL);
  ASSERT_TRUE(normal_trb->ISP());
  ASSERT_TRUE(normal_trb->NO_SNOOP());
  // Status
  trb = FakeTRB::get(trb->next);
  auto status_trb = static_cast<Status*>(static_cast<TRB*>(trb));
  ASSERT_EQ(status_trb->DIRECTION(), 0UL);
  ASSERT_EQ(status_trb->INTERRUPTER(), 0UL);
  ASSERT_TRUE(status_trb->IOC());
  // Interrupt on completion
  TransferRing* ring = reinterpret_cast<TransferRing*>(initial_trb->ptr);
  std::unique_ptr<TRBContext> context;
  ring->CompleteTRB(trb, &context);
  std::get<Request>(*context->request).Complete(ZX_OK, sizeof(void*));
  ASSERT_TRUE(invoked);
}

TEST_F(XhciMmioHarness, QueueNormalRequest) {
  ConnectDevice(1, USB_SPEED_FULL);
  EnableEndpoint(0, 1, true);
  bool rang = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 1 && target == 3) {
      rang = true;
    }
  });

  std::optional<TestRequest> request;
  bool invoked = false;
  AllocateRequest(&request, 0, zx_system_get_page_size() * 2, 1 | 0x80, [&](TestRequest request) {
    invoked = true;
    void** parameters;
    request.Mmap(reinterpret_cast<void**>(&parameters));
    EXPECT_EQ(*parameters, parameters);
  });

  RequestQueue(std::move(*request));
  ASSERT_TRUE(rang);
  // Find slot context pointer in address device command
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  ASSERT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  [[maybe_unused]] auto control =
      static_cast<unsigned char*>(reinterpret_cast<FakeVMO*>(cr->ptr)->virt);
  auto endpoint_context = reinterpret_cast<EndpointContext*>(control + (64 * 4));
  auto ring_phys =
      static_cast<zx_paddr_t>(static_cast<uint64_t>(endpoint_context->dequeue_pointer_a) |
                              (static_cast<uint64_t>(endpoint_context->dequeue_pointer_b) << 32)) &
      (~1);

  auto trb_start = FakeTRB::get(ring_phys);

  // Data (page 0)
  auto trb = FakeTRB::get(trb_start->next)->contig.data();
  auto data_trb = static_cast<Normal*>(static_cast<TRB*>(trb));
  ASSERT_EQ(Control::FromTRB(data_trb).Type(), Control::Normal);
  ASSERT_EQ(data_trb->IOC(), 0UL);
  ASSERT_EQ(data_trb->ISP(), 1UL);
  ASSERT_EQ(data_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(data_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(data_trb->SIZE(), 1UL);
  ASSERT_TRUE(data_trb->NO_SNOOP());
  void** virt = reinterpret_cast<void**>(FakeTRB::get(static_cast<zx_paddr_t>(data_trb->ptr))->ptr);
  *virt = virt;

  // Data (page 1, contiguous)
  trb++;
  data_trb = static_cast<Normal*>(static_cast<TRB*>(trb));
  ASSERT_EQ(data_trb->IOC(), 1UL);
  ASSERT_EQ(data_trb->ISP(), 1UL);
  ASSERT_EQ(data_trb->INTERRUPTER(), 0UL);
  ASSERT_EQ(data_trb->LENGTH(), zx_system_get_page_size());
  ASSERT_EQ(data_trb->SIZE(), 0UL);
  ASSERT_TRUE(data_trb->NO_SNOOP());

  // Interrupt on completion
  TransferRing* ring = reinterpret_cast<TransferRing*>(trb_start->ptr);
  std::unique_ptr<TRBContext> context;
  ring->CompleteTRB(trb, &context);
  std::get<Request>(*context->request).Complete(ZX_OK, sizeof(void*));
  ASSERT_TRUE(invoked);
}

TEST_F(XhciMmioHarness, CancelAllOnDisabledEndpoint) {
  ConnectDevice(1, USB_SPEED_HIGH);
  zx_status_t cancel_status;
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  EXPECT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  CommandCompletionEvent event;
  event.set_CompletionCode(CommandCompletionEvent::Success);
  EXPECT_OK(CompleteCommand(cr, &event));
  bool got_stop_endpoint = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      Control control = Control::FromTRB(cr);
      switch (control.Type()) {
        case Control::StopEndpointCommand: {
          auto cancel_command = reinterpret_cast<StopEndpoint*>(cr);
          EXPECT_EQ(cancel_command->ENDPOINT(), 2UL);
          EXPECT_EQ(cancel_command->SLOT(), 1UL);
          got_stop_endpoint = true;
          EXPECT_OK(CompleteCommand(cr, &event));
        } break;
      }
    }
  });
  cancel_status = driver_test().driver()->UsbHciCancelAll(0, 1);
  EXPECT_TRUE(got_stop_endpoint);
  EXPECT_EQ(cancel_status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, ResetEndpointTestSuccessCase) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);
  uint64_t paddr;
  {
    auto state = driver_test().driver()->GetDeviceState(0);
    ASSERT_TRUE(state);
    fbl::AutoLock l(&state->transaction_lock());
    auto& transfer_ring = state->GetEndpoint(0).transfer_ring();
    transfer_ring.set_stall(true);
    paddr = transfer_ring.PeekCommandRingControlRegister(0).value().reg_value();
  }

  zx_status_t reset_status;
  auto cr = crcr_next();
  Control control_trb = Control::FromTRB(cr);
  EXPECT_EQ(control_trb.Type(), Control::AddressDeviceCommand);
  CommandCompletionEvent event;
  event.set_CompletionCode(CommandCompletionEvent::Success);
  EXPECT_OK(CompleteCommand(cr, &event));
  cr = FakeTRB::get(cr->next);
  control_trb = Control::FromTRB(cr);
  EXPECT_EQ(control_trb.Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &event));

  bool got_reset_endpoint = false;
  bool got_set_tr_dequeue_ptr = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      Control control = Control::FromTRB(cr);
      switch (control.Type()) {
        case Control::ResetEndpointCommand: {
          auto reset_command = reinterpret_cast<ResetEndpoint*>(cr);
          EXPECT_EQ(reset_command->ENDPOINT(), 2UL);
          EXPECT_EQ(reset_command->SLOT(), 1UL);
          got_reset_endpoint = true;
          EXPECT_OK(CompleteCommand(cr, &event));
        } break;
        case Control::SetTrDequeuePointerCommand: {
          // ResetEndpoint should be sent prior to SetTrDequeuePointer
          EXPECT_TRUE(got_reset_endpoint);
          auto set_cmd = reinterpret_cast<SetTRDequeuePointer*>(cr);
          EXPECT_EQ(set_cmd->ENDPOINT(), 2UL);
          EXPECT_EQ(set_cmd->ptr, paddr);
          EXPECT_OK(CompleteCommand(cr, &event));
          got_set_tr_dequeue_ptr = true;
        } break;
      }
    }
  });

  reset_status = driver_test().driver()->UsbHciResetEndpoint(0, 1);

  EXPECT_TRUE(got_reset_endpoint);
  EXPECT_TRUE(got_set_tr_dequeue_ptr);
  EXPECT_OK(reset_status);
}

TEST_F(XhciMmioHarness, ResetEndpointFailsIfNotStalled) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);
  {
    auto state = driver_test().driver()->GetDeviceState(0);
    ASSERT_TRUE(state);
    fbl::AutoLock l(&state->transaction_lock());
    state->GetEndpoint(0).transfer_ring().set_stall(false);
  }
  ASSERT_EQ(driver_test().driver()->UsbHciResetEndpoint(0, 1), ZX_ERR_INVALID_ARGS);
}

// ResetEndpoint propagates TR dequeue pointer failure to the caller (doorbell completions are
// synchronous in this harness).
TEST_F(XhciMmioHarness, ResetEndpointSetDequeuePointerFails) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);
  {
    auto state = driver_test().driver()->GetDeviceState(0);
    ASSERT_TRUE(state);
    fbl::AutoLock l(&state->transaction_lock());
    state->GetEndpoint(0).transfer_ring().set_stall(true);
  }

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  CommandCompletionEvent fail_event;
  fail_event.set_CompletionCode(CommandCompletionEvent::ContextStateError);
  auto cr = crcr_next();
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));
  cr = FakeTRB::get(cr->next);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  bool got_reset_endpoint = false;
  bool got_set_tr_dequeue_ptr = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell != 0) {
      return;
    }
    cr = FakeTRB::get(cr->next);
    switch (Control::FromTRB(cr).Type()) {
      case Control::ResetEndpointCommand:
        got_reset_endpoint = true;
        EXPECT_OK(CompleteCommand(cr, &success_event));
        break;
      case Control::SetTrDequeuePointerCommand:
        EXPECT_TRUE(got_reset_endpoint);
        got_set_tr_dequeue_ptr = true;
        EXPECT_OK(CompleteCommand(cr, &fail_event));
        break;
    }
  });

  zx_status_t reset_status = driver_test().driver()->UsbHciResetEndpoint(0, 1);

  EXPECT_TRUE(got_reset_endpoint);
  EXPECT_TRUE(got_set_tr_dequeue_ptr);
  EXPECT_EQ(reset_status, ZX_ERR_IO);
}

// CancelAll propagates StopEndpoint command failure to the caller.
TEST_F(XhciMmioHarness, CancelAllStopEndpointFails) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, false);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  CommandCompletionEvent fail_event;
  fail_event.set_CompletionCode(CommandCompletionEvent::ContextStateError);
  auto cr = crcr_next();
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));
  cr = FakeTRB::get(cr->next);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  bool got_stop_endpoint = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell != 0) {
      return;
    }
    cr = FakeTRB::get(cr->next);
    if (Control::FromTRB(cr).Type() == Control::StopEndpointCommand) {
      got_stop_endpoint = true;
      EXPECT_OK(CompleteCommand(cr, &fail_event));
    }
  });

  zx_status_t cancel_status = driver_test().driver()->UsbHciCancelAll(0, 1);

  EXPECT_TRUE(got_stop_endpoint);
  EXPECT_EQ(cancel_status, ZX_ERR_IO);
}

TEST_F(XhciMmioHarness, GetMaxDeviceCount) { ASSERT_EQ(GetMaxDeviceCount(), 34UL); }

namespace {

// NOLINTNEXTLINE(performance-unnecessary-value-param)
TEST_F(XhciMmioHarness, StopWithHCHaltedTimeout) {
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  auto poll_count = std::make_shared<std::atomic<size_t>>(0);
  auto allow_halt = std::make_shared<std::atomic<bool>>(false);
  // Restore HCHalted == 1 on scope exit (including early ASSERT failures) so TearDown() and
  // ~UsbXhci() do not self-terminate to quarantine pinned DMA pages.
  auto restore_halt =
      fit::defer([allow_halt] { allow_halt->store(true, std::memory_order_relaxed); });
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbSts].SetReadCallback([=] {
      poll_count->fetch_add(1, std::memory_order_relaxed);
      // Simulate MMIO unmapping or unhalted controller where HCHalted remains 0 during Stop().
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(allow_halt->load(std::memory_order_relaxed) ? 1 : 0);
      return sts.reg_value();
    });
  });

  libsync::Completion stop_done;
  driver_test().driver()->Stop(fdf::StopCompleter([&](zx::result<> result) {
    EXPECT_STATUS(result.status_value(), ZX_ERR_TIMED_OUT);
    stop_done.Signal();
  }));
  stop_done.Wait();
  EXPECT_GT(poll_count->load(std::memory_order_relaxed), 1u);
}

TEST_F(XhciMmioHarness, RunSynchronouslyDoesNotAbortPendingCommandsWhenRunning) {
  // Queue a command TRB on the command ring via ConnectDevice.
  ConnectDevice(1, USB_SPEED_HIGH);
  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);

  // Schedule a promise via RunSynchronously that takes 300ms to resolve while the controller is
  // healthy and running (!IsHaltedOrStopping()).
  fpromise::bridge<void, zx_status_t> slow_bridge;
  std::thread completer_thread([completer = std::move(slow_bridge.completer)] mutable {
    zx::nanosleep(zx::deadline_after(zx::msec(300)));
    completer.complete_ok();
  });

  zx_status_t sync_status =
      driver_test().driver()->RunSynchronously(0, slow_bridge.consumer.promise().box());
  completer_thread.join();

  // RunSynchronously must wait for the promise to complete with ZX_OK and must NOT abort pending
  // commands on the command ring after 250ms while the controller is still running.
  EXPECT_OK(sync_status);

  CommandCompletionEvent event;
  event.set_CompletionCode(CommandCompletionEvent::Success);
  EXPECT_OK(CompleteCommand(cr, &event));

  // Also verify that if the controller halts while RunSynchronously is waiting on a non-command
  // promise, early return does not cause stack-use-after-return when the promise later resolves.
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbSts].SetReadCallback([=] {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(1);
      return sts.reg_value();
    });
  });

  fpromise::bridge<void, zx_status_t> halted_bridge;
  EXPECT_EQ(driver_test().driver()->RunSynchronously(0, halted_bridge.consumer.promise().box()),
            ZX_ERR_IO_NOT_PRESENT);
  // Resolving the promise after RunSynchronously has returned must safely update heap-shared state
  // without touching a popped stack frame.
  halted_bridge.completer.complete_ok();
  driver_test().driver()->RunUntilIdle(0);
}

TEST_F(XhciMmioHarness, DeviceOnlineUsesOneBasedPortIndex) {
  // GetPortState() uses 1-based port indexing matching the xHCI specification.
  std::optional<HubInfo> hub;
  driver_test().driver()->SetDeviceInformation(1, 1, hub);
  driver_test().driver()->GetPortState(1).is_USB3 = true;
  driver_test().driver()->GetPortState(2).is_USB3 = false;

  bus_.reset();
  EXPECT_OK(driver_test().driver()->DeviceOnline(1, 1, USB_SPEED_SUPER));
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this]() { bus_.wait(); }));
  ASSERT_EQ(bus_.devices().count(0), 1u);
  EXPECT_EQ(bus_.devices().at(0).hub_id, GetMaxDeviceCount() - 1);

  // Also verify port 2 (USB 2.0) routes to the USB 2.0 root hub (GetMaxDeviceCount()).
  driver_test().driver()->SetDeviceInformation(2, 2, hub);
  bus_.reset();
  EXPECT_OK(driver_test().driver()->DeviceOnline(2, 2, USB_SPEED_HIGH));
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this]() { bus_.wait(); }));
  ASSERT_EQ(bus_.devices().count(1), 1u);
  EXPECT_EQ(bus_.devices().at(1).hub_id, GetMaxDeviceCount() - 2);
}

TEST_F(XhciMmioHarness, GetPortStateUsesOneBasedPortIndex) {
  driver_test().driver()->GetPortState(1).slot_id = 42;
  driver_test().driver()->GetPortState(2).slot_id = 84;

  EXPECT_EQ(driver_test().driver()->GetPortState(1).slot_id, 42);
  EXPECT_EQ(driver_test().driver()->GetPortState(2).slot_id, 84);
}

TEST_F(XhciMmioHarness, DeviceOfflineWithBoundFidlEndpointAndPendingRequest) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(0, 1, true);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cr = crcr_next();
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));
  cr = FakeTRB::get(cr->next);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::ConfigureEndpointCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  zx::result hci_client_end =
      driver_test().Connect<fuchsia_hardware_usb_hci::UsbHciService::Device>();
  ASSERT_OK(hci_client_end);
  auto [ep_client_end, ep_server_end] =
      fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_client(std::move(*hci_client_end));
    auto connect_res = hci_client->ConnectToEndpoint(0, 1U | 0x80U, std::move(ep_server_end));
    ASSERT_TRUE(connect_res.ok());
    ASSERT_TRUE(connect_res->is_ok());
  }));

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(std::move(ep_client_end));

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    std::vector<fuchsia_hardware_usb_endpoint::VmoInfo> vmo_info;
    vmo_info.emplace_back(
        std::move(fuchsia_hardware_usb_endpoint::VmoInfo().id(1).size(zx_system_get_page_size())));
    auto reg_res = ep_client->RegisterVmos({std::move(vmo_info)});
    ASSERT_TRUE(reg_res.is_ok());

    std::vector<fuchsia_hardware_usb_request::Request> requests;
    requests.emplace_back()
        .defer_completion(false)
        .information(fuchsia_hardware_usb_request::RequestInfo::WithBulk(
            fuchsia_hardware_usb_request::BulkRequestInfo()))
        .data()
        .emplace()
        .emplace_back()
        .offset(0)
        .size(zx_system_get_page_size())
        .buffer(fuchsia_hardware_usb_request::Buffer::WithVmoId(1));
    auto queue_res = ep_client->QueueRequests(std::move(requests));
    ASSERT_TRUE(queue_res.is_ok());
  }));

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending_cmds = driver_test().driver()->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending_cmds) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  RunDeviceOffline(1);
  EXPECT_FALSE(driver_test().driver()->GetDeviceState(0));

  // Verify FIDL calls and channel unbind after DeviceOffline dropped device_state_[0] do not
  // trigger a Use-After-Free or null pointer dereference.
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    auto cancel_res = ep_client->CancelAll();
    ASSERT_TRUE(cancel_res.is_error());
    ASSERT_TRUE(cancel_res.error_value().is_domain_error());
    EXPECT_EQ(cancel_res.error_value().domain_error(), ZX_ERR_IO_NOT_PRESENT);
    ep_client = {};
  }));
  driver_test().runtime().RunUntilIdle();
}

struct __PACKED TestFullConfigDescriptor {
  usb_configuration_descriptor_t config;
  usb_interface_descriptor_t interface;
  usb_endpoint_descriptor_t ep_in;
  usb_endpoint_descriptor_t ep_out;
};

constexpr TestFullConfigDescriptor kTestFullConfigDesc = {
    .config =
        {
            .b_length = sizeof(usb_configuration_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration),
            .w_total_length = htole16(sizeof(TestFullConfigDescriptor)),
            .b_num_interfaces = 1,
            .b_configuration_value = 1,
            .i_configuration = 0,
            .bm_attributes = 0xC0,
            .b_max_power = 50,
        },
    .interface =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kInterface),
            .b_interface_number = 0,
            .b_alternate_setting = 0,
            .b_num_endpoints = 2,
            .b_interface_class = USB_CLASS_VENDOR,
            .b_interface_sub_class = 0,
            .b_interface_protocol = 0,
            .i_interface = 0,
        },
    .ep_in =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x81,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = htole16(512),
            .b_interval = 0,
        },
    .ep_out =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
            .b_endpoint_address = 0x02,
            .bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kBulk),
            .w_max_packet_size = htole16(512),
            .b_interval = 0,
        },
};

constexpr usb_device_descriptor_t kTestDeviceDesc = {
    .b_length = sizeof(usb_device_descriptor_t),
    .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice),
    .bcd_usb = htole16(0x0200),
    .b_device_class = 0,
    .b_device_sub_class = 0,
    .b_device_protocol = 0,
    .b_max_packet_size0 = 64,
    .id_vendor = htole16(0x18d1),
    .id_product = htole16(0x5026),
    .bcd_device = htole16(0x0100),
    .i_manufacturer = 0,
    .i_product = 0,
    .i_serial_number = 0,
    .b_num_configurations = 1,
};

const usb_hci_protocol_ops_t kTestXhciBanjoOps = {
    .request_queue =
        [](void* ctx, usb_request_t* usb_request,
           const usb_request_complete_callback_t* complete_cb) {
          static_cast<UsbXhci*>(ctx)->UsbHciRequestQueue(usb_request, complete_cb);
        },
    .set_bus_interface =
        [](void* ctx, const usb_bus_interface_protocol_t* bus_intf) {
          static_cast<UsbXhci*>(ctx)->UsbHciSetBusInterface(bus_intf);
        },
    .get_max_device_count =
        [](void* ctx) { return static_cast<UsbXhci*>(ctx)->UsbHciGetMaxDeviceCount(); },
    .enable_endpoint =
        [](void* ctx, uint32_t device_id, const usb_endpoint_descriptor_t* ep_desc,
           const usb_ss_ep_comp_descriptor_t* ss_com_desc, bool enable) {
          return static_cast<UsbXhci*>(ctx)->UsbHciEnableEndpoint(device_id, ep_desc, ss_com_desc,
                                                                  enable);
        },
    .get_current_frame =
        [](void* ctx) { return static_cast<UsbXhci*>(ctx)->UsbHciGetCurrentFrame(); },
    .configure_hub =
        [](void* ctx, uint32_t device_id, usb_speed_t speed, const usb_hub_descriptor_t* desc,
           bool multi_tt) {
          return static_cast<UsbXhci*>(ctx)->UsbHciConfigureHub(device_id, speed, desc, multi_tt);
        },
    .hub_device_added =
        [](void* ctx, uint32_t device_id, uint32_t port, usb_speed_t speed) {
          return static_cast<UsbXhci*>(ctx)->UsbHciHubDeviceAdded(device_id, port, speed);
        },
    .hub_device_removed =
        [](void* ctx, uint32_t device_id, uint32_t port) {
          return static_cast<UsbXhci*>(ctx)->UsbHciHubDeviceRemoved(device_id, port);
        },
    .hub_device_reset =
        [](void* ctx, uint32_t device_id, uint32_t port) {
          return static_cast<UsbXhci*>(ctx)->UsbHciHubDeviceReset(device_id, port);
        },
    .reset_endpoint =
        [](void* ctx, uint32_t device_id, uint8_t ep_address) {
          return static_cast<UsbXhci*>(ctx)->UsbHciResetEndpoint(device_id, ep_address);
        },
    .reset_device =
        [](void* ctx, uint32_t hub_address, uint32_t device_id) {
          return static_cast<UsbXhci*>(ctx)->UsbHciResetDevice(hub_address, device_id);
        },
    .get_max_transfer_size =
        [](void* ctx, uint32_t device_id, uint8_t ep_address) {
          return static_cast<UsbXhci*>(ctx)->UsbHciGetMaxTransferSize(device_id, ep_address);
        },
    .cancel_all =
        [](void* ctx, uint32_t device_id, uint8_t ep_address) {
          return static_cast<UsbXhci*>(ctx)->UsbHciCancelAll(device_id, ep_address);
        },
    .get_request_size =
        [](void* ctx) { return static_cast<UsbXhci*>(ctx)->UsbHciGetRequestSize(); },
};

// Implements the usb-bus UsbDevice role (EP0 control enumeration + fuchsia.hardware.usb.Usb server
// forwarding EnableEndpoint via UsbHci Banjo and ConnectToEndpoint via UsbHci FIDL).
class TestUsbBusDevice : public fidl::Server<fuchsia_hardware_usb::Usb> {
 public:
  TestUsbBusDevice(ddk::UsbHciProtocolClient hci_banjo,
                   fidl::ClientEnd<fuchsia_hardware_usb_hci::UsbHci> hci_fidl, uint32_t device_id,
                   usb_speed_t speed)
      : hci_banjo_(hci_banjo),
        hci_fidl_(std::move(hci_fidl)),
        device_id_(device_id),
        speed_(speed),
        parent_req_size_(hci_banjo_.GetRequestSize()) {}

  zx_status_t Init() {
    size_t actual = 0;
    zx_status_t status = Control(
        kStandardDeviceIn, fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
        static_cast<uint16_t>(
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice)) << 8u),
        0, nullptr, 0, &device_desc_, sizeof(device_desc_), &actual);
    if (status != ZX_OK || actual != sizeof(device_desc_)) {
      return status != ZX_OK ? status : ZX_ERR_IO;
    }

    usb_configuration_descriptor_t cfg_hdr = {};
    status = Control(
        kStandardDeviceIn, fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
        static_cast<uint16_t>(
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration))
            << 8u),
        0, nullptr, 0, &cfg_hdr, sizeof(cfg_hdr), &actual);
    if (status != ZX_OK || actual != sizeof(cfg_hdr)) {
      return status != ZX_OK ? status : ZX_ERR_IO;
    }

    config_desc_.resize(letoh16(cfg_hdr.w_total_length));
    status = Control(
        kStandardDeviceIn, fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor),
        static_cast<uint16_t>(
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration))
            << 8u),
        0, nullptr, 0, config_desc_.data(), config_desc_.size(), &actual);
    if (status != ZX_OK || actual != config_desc_.size()) {
      return status != ZX_OK ? status : ZX_ERR_IO;
    }

    return Control(kStandardDeviceOut,
                   fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration),
                   cfg_hdr.b_configuration_value, 0, nullptr, 0, nullptr, 0, nullptr);
  }

  void Bind(async_dispatcher_t* dispatcher, fidl::ServerEnd<fuchsia_hardware_usb::Usb> server_end) {
    bindings_.AddBinding(dispatcher, std::move(server_end), this, fidl::kIgnoreBindingClosure);
  }

  void ConnectToEndpoint(ConnectToEndpointRequest& request,
                         ConnectToEndpointCompleter::Sync& completer) override {
    auto res = hci_fidl_->ConnectToEndpoint(device_id_, request.ep_addr(), std::move(request.ep()));
    if (!res.ok()) {
      completer.Reply(fit::as_error(res.status()));
      return;
    }
    if (res->is_error()) {
      completer.Reply(fit::as_error(res->error_value()));
      return;
    }
    completer.Reply(fit::ok());
  }

  void GetDeviceSpeed(GetDeviceSpeedCompleter::Sync& completer) override {
    completer.Reply({{.speed = static_cast<fdescriptor::UsbSpeed>(speed_)}});
  }
  void GetDeviceId(GetDeviceIdCompleter::Sync& completer) override {
    completer.Reply({{.device_id = device_id_}});
  }
  void GetDeviceDescriptor(GetDeviceDescriptorCompleter::Sync& completer) override {
    fdescriptor::UsbDeviceDescriptor desc{{
        .b_length = device_desc_.b_length,
        .b_descriptor_type = device_desc_.b_descriptor_type,
        .bcd_usb = le16toh(device_desc_.bcd_usb),
        .b_device_class = device_desc_.b_device_class,
        .b_device_sub_class = device_desc_.b_device_sub_class,
        .b_device_protocol = device_desc_.b_device_protocol,
        .b_max_packet_size0 = device_desc_.b_max_packet_size0,
        .id_vendor = le16toh(device_desc_.id_vendor),
        .id_product = le16toh(device_desc_.id_product),
        .bcd_device = le16toh(device_desc_.bcd_device),
        .i_manufacturer = device_desc_.i_manufacturer,
        .i_product = device_desc_.i_product,
        .i_serial_number = device_desc_.i_serial_number,
        .b_num_configurations = device_desc_.b_num_configurations,
    }};
    completer.Reply({{.descriptor = desc}});
  }
  void GetConfigurationDescriptor(GetConfigurationDescriptorRequest& request,
                                  GetConfigurationDescriptorCompleter::Sync& completer) override {
    completer.Reply(fit::ok(
        fuchsia_hardware_usb::UsbGetConfigurationDescriptorResponse{{.descriptor = config_desc_}}));
  }
  void GetConfiguration(GetConfigurationCompleter::Sync& completer) override {
    completer.Reply({{.configuration = 1}});
  }
  void SetConfiguration(SetConfigurationRequest& request,
                        SetConfigurationCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }
  void SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) override {
    completer.Reply(fit::ok());
  }
  void ControlTransfer(ControlTransferRequest& request,
                       ControlTransferCompleter::Sync& completer) override {
    completer.Reply(fit::as_error(ZX_ERR_NOT_SUPPORTED));
  }
  void EnableEndpoint(EnableEndpointRequest& request,
                      EnableEndpointCompleter::Sync& completer) override {
    const usb_endpoint_descriptor_t ep_desc = {
        .b_length = request.ep_desc().b_length(),
        .b_descriptor_type = request.ep_desc().b_descriptor_type(),
        .b_endpoint_address = request.ep_desc().b_endpoint_address(),
        .bm_attributes = request.ep_desc().bm_attributes(),
        .w_max_packet_size = htole16(request.ep_desc().w_max_packet_size()),
        .b_interval = request.ep_desc().b_interval(),
    };
    zx_status_t status = hci_banjo_.EnableEndpoint(device_id_, &ep_desc, nullptr, request.enable());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok());
  }
  void ResetEndpoint(ResetEndpointRequest& request,
                     ResetEndpointCompleter::Sync& completer) override {
    zx_status_t status = hci_banjo_.ResetEndpoint(device_id_, request.ep_address());
    if (status != ZX_OK) {
      completer.Reply(fit::as_error(status));
      return;
    }
    completer.Reply(fit::ok());
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_usb::Usb> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  zx_status_t Control(uint8_t request_type, uint8_t request, uint16_t value, uint16_t index,
                      const void* write_buf, size_t write_size, void* read_buf, size_t read_size,
                      size_t* out_actual) {
    const bool is_out = usb_request_is_out(request_type);
    const size_t length = is_out ? write_size : read_size;
    std::optional<usb::Request<void>> req;
    zx_status_t status = usb::Request<void>::Alloc(&req, length, 0, parent_req_size_);
    if (status != ZX_OK || !req.has_value()) {
      return status != ZX_OK ? status : ZX_ERR_INTERNAL;
    }
    usb::Request<void>& req_val = *req;
    req_val.request()->setup.bm_request_type = request_type;
    req_val.request()->setup.b_request = request;
    req_val.request()->setup.w_value = value;
    req_val.request()->setup.w_index = index;
    req_val.request()->setup.w_length = static_cast<uint16_t>(length);
    req_val.request()->header.device_id = device_id_;
    req_val.request()->header.ep_address = 0;
    req_val.request()->header.length = length;
    if (is_out && length > 0) {
      ZX_ASSERT(req_val.CopyTo(write_buf, length, 0) == static_cast<ssize_t>(length));
    }
    sync_completion_t completion;
    usb_request_complete_callback_t complete_cb = {
        .callback =
            [](void* ctx, usb_request_t*) {
              sync_completion_signal(static_cast<sync_completion_t*>(ctx));
            },
        .ctx = &completion,
    };
    hci_banjo_.RequestQueue(req_val.request(), &complete_cb);
    sync_completion_wait(&completion, ZX_TIME_INFINITE);
    status = req_val.request()->response.status;
    if (status == ZX_OK && !is_out) {
      const size_t actual = req_val.request()->response.actual;
      if (length > 0 && actual > 0) {
        auto copy_res = req_val.CachedCopyFrom(read_buf, actual, 0);
        ZX_ASSERT(copy_res.is_ok() && copy_res.value() == actual);
      }
      if (out_actual) {
        *out_actual = actual;
      }
    }
    req_val.Release();
    return status;
  }

  ddk::UsbHciProtocolClient hci_banjo_;
  fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_fidl_;
  uint32_t device_id_;
  usb_speed_t speed_;
  size_t parent_req_size_;
  usb_device_descriptor_t device_desc_ = {};
  std::vector<uint8_t> config_desc_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb::Usb> bindings_;
};

// Multi-driver integration test covering UsbXhci <-> UsbBus/UsbDevice <-> FIDL Usb/Endpoint
// across repeated M1/M4 attach and M2 unplug cycles with in-flight FIDL and Banjo transfers.
TEST_F(XhciMmioHarness, UsbBusAndUsbDeviceMultiCycleHotplugWithInFlightFidlEndpoint) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending_cmds = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending_cmds) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
      return;
    }
    // Auto-complete EP0 control requests queued during device enumeration (GET_DESCRIPTOR and
    // SET_CONFIGURATION) so enumeration succeeds end-to-end.
    if (target == 1) {
      auto state = xhci->GetDeviceState(doorbell - 1);
      if (!state) {
        return;
      }
      fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> pending_trbs;
      Endpoint* ep0 = nullptr;
      {
        fbl::AutoLock _(&state->transaction_lock());
        ep0 = &state->GetEndpoint();
        pending_trbs = ep0->transfer_ring().TakePendingTRBs();
      }
      for (auto& trb : pending_trbs) {
        if (!trb.request.has_value()) {
          continue;
        }
        size_t actual = 0;
        if (std::holds_alternative<Request>(*trb.request)) {
          auto& req = std::get<Request>(*trb.request);
          const usb_setup_t& setup = req.request()->setup;
          if (setup.b_request == fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor)) {
            const uint8_t desc_type =
                static_cast<uint8_t>(static_cast<uint32_t>(setup.w_value) >> 8u);
            if (desc_type == fidl::ToUnderlying(fdescriptor::DescriptorType::kDevice)) {
              actual = std::min<size_t>(setup.w_length, sizeof(kTestDeviceDesc));
              EXPECT_EQ(static_cast<size_t>(req.CopyTo(&kTestDeviceDesc, actual, 0)), actual);
            } else if (desc_type ==
                       fidl::ToUnderlying(fdescriptor::DescriptorType::kConfiguration)) {
              actual = std::min<size_t>(setup.w_length, sizeof(kTestFullConfigDesc));
              EXPECT_EQ(static_cast<size_t>(req.CopyTo(&kTestFullConfigDesc, actual, 0)), actual);
            }
          }
        }
        ep0->RequestComplete(ZX_OK, actual, std::move(*trb.request));
      }
    }
  });

  usb_hci_protocol_t hci_proto = {
      .ops = &kTestXhciBanjoOps,
      .ctx = xhci,
  };
  ddk::UsbHciProtocolClient hci_banjo_client(&hci_proto);

  auto usb_bus_dispatcher = driver_test().runtime().StartBackgroundDispatcher();

  for (size_t cycle = 0; cycle < 2; ++cycle) {
    SCOPED_TRACE(::testing::Message() << "Hotplug cycle " << cycle);

    // 1. Simulate physical USB device attach on port 1.
    FakeUsbDevice fake_dev = ConnectDevice(1, USB_SPEED_HIGH);
    const uint32_t device_id = fake_dev.device_id;
    const uint8_t slot_id = static_cast<uint8_t>(device_id + 1);

    zx::result hci_fidl_client =
        driver_test().Connect<fuchsia_hardware_usb_hci::UsbHciService::Device>();
    ASSERT_OK(hci_fidl_client);

    // 2. Enumerate the attached device over EP0 and bind its fuchsia.hardware.usb.Usb server.
    std::unique_ptr<TestUsbBusDevice> bus_device;
    auto [usb_client_end, usb_server_end] = fidl::Endpoints<fuchsia_hardware_usb::Usb>::Create();
    EXPECT_OK(fdf::RunOnDispatcherSync(usb_bus_dispatcher->async_dispatcher(), [&] {
      bus_device = std::make_unique<TestUsbBusDevice>(hci_banjo_client, std::move(*hci_fidl_client),
                                                      device_id, fake_dev.speed);
      ASSERT_OK(bus_device->Init());
      bus_device->Bind(usb_bus_dispatcher->async_dispatcher(), std::move(usb_server_end));
    }));

    // 3. Connect to Bulk IN endpoint 0x81 through the fuchsia.hardware.usb.Usb server (matching
    // starnix/modules/usbfs), which enables the endpoint in UsbXhci and binds the FIDL Endpoint
    // server in xhci-endpoint.cc.
    auto [ep_client_end, ep_server_end] =
        fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
    fidl::SyncClient<fuchsia_hardware_usb::Usb> usb_client(std::move(usb_client_end));

    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
      auto desc_res = usb_client->GetDeviceDescriptor();
      ASSERT_TRUE(desc_res.is_ok());

      fdescriptor::UsbEndpointDescriptor ep_in_desc{{
          .b_length = sizeof(usb_endpoint_descriptor_t),
          .b_descriptor_type = static_cast<uint8_t>(fdescriptor::DescriptorType::kEndpoint),
          .b_endpoint_address = 0x81,
          .bm_attributes = std::to_underlying(fdescriptor::EndpointType::kBulk),
          .w_max_packet_size = 512,
          .b_interval = 0,
      }};
      fdescriptor::UsbSsEpCompDescriptor ss_comp{};
      auto enable_res = usb_client->EnableEndpoint(
          {{.ep_desc = ep_in_desc, .ss_com_desc = ss_comp, .enable = true}});
      ASSERT_TRUE(enable_res.is_ok());

      auto connect_res =
          usb_client->ConnectToEndpoint({{.ep_addr = 0x81, .ep = std::move(ep_server_end)}});
      ASSERT_TRUE(connect_res.is_ok());
    }));

    fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> ep_client(std::move(ep_client_end));

    // 4. Register VMOs and queue an in-flight FIDL bulk request on 0x81 (pins BTI PMT in xhci).
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
      std::vector<fuchsia_hardware_usb_endpoint::VmoInfo> vmo_info;
      vmo_info.emplace_back(std::move(
          fuchsia_hardware_usb_endpoint::VmoInfo().id(1).size(zx_system_get_page_size())));
      auto reg_res = ep_client->RegisterVmos({std::move(vmo_info)});
      ASSERT_TRUE(reg_res.is_ok());

      std::vector<fuchsia_hardware_usb_request::Request> requests;
      requests.emplace_back()
          .defer_completion(false)
          .information(fuchsia_hardware_usb_request::RequestInfo::WithBulk(
              fuchsia_hardware_usb_request::BulkRequestInfo()))
          .data()
          .emplace()
          .emplace_back()
          .offset(0)
          .size(zx_system_get_page_size())
          .buffer(fuchsia_hardware_usb_request::Buffer::WithVmoId(1));
      auto queue_res = ep_client->QueueRequests(std::move(requests));
      ASSERT_TRUE(queue_res.is_ok());
    }));

    // 5. Also enable Bulk OUT endpoint 0x02 and queue an in-flight Banjo usb::Request with a VMO
    // (which UsbXhci pins via PhysMap(hci_->bti())) so both FIDL and Banjo requests are pending on
    // transfer rings when DeviceOffline runs.
    EnableEndpoint(device_id, 2, false);
    driver_test().driver()->RunUntilIdle();

    libsync::Completion banjo_req_completed;
    zx_status_t banjo_completion_status = ZX_OK;
    std::optional<usb::Request<void>> banjo_req;
    ASSERT_OK(usb::Request<void>::Alloc(&banjo_req, zx_system_get_page_size(), 0x02,
                                        driver_test().driver()->UsbHciGetRequestSize()));
    ASSERT_TRUE(banjo_req.has_value());
    usb::Request<void>& banjo_req_val = *banjo_req;
    banjo_req_val.request()->header.device_id = device_id;
    banjo_req_val.request()->header.ep_address = 0x02;
    banjo_req_val.request()->header.length = zx_system_get_page_size();
    usb_request_complete_callback_t banjo_cb = {
        .callback =
            [](void* ctx, usb_request_t* req) {
              auto* state_pair = static_cast<std::pair<zx_status_t*, libsync::Completion*>*>(ctx);
              *state_pair->first = req->response.status;
              usb::Request<void> owned_req(req, sizeof(usb_request_t));
              state_pair->second->Signal();
            },
        .ctx = nullptr,
    };
    std::pair<zx_status_t*, libsync::Completion*> cb_ctx(&banjo_completion_status,
                                                         &banjo_req_completed);
    banjo_cb.ctx = &cb_ctx;
    driver_test().driver()->UsbHciRequestQueue(banjo_req_val.take(), &banjo_cb);

    // 6. Simulate physical cable unplug / role transition while both FIDL and Banjo transfers are
    // in flight and the FIDL Endpoint channel is still bound.
    RunDeviceOffline(slot_id);
    EXPECT_FALSE(driver_test().driver()->GetDeviceState(device_id));

    // Both the in-flight Banjo request and the in-flight FIDL request must have been unmapped and
    // completed with ZX_ERR_IO_NOT_PRESENT by UsbXhci::DeviceOffline.
    EXPECT_OK(banjo_req_completed.Wait(zx::sec(5)));
    EXPECT_EQ(banjo_completion_status, ZX_ERR_IO_NOT_PRESENT);

    // 7. Tear down the bus device node and close the FIDL Endpoint channel AFTER
    // device_state_[device_id] has already been dropped by UsbXhci::DeviceOffline.
    EXPECT_OK(fdf::RunOnDispatcherSync(usb_bus_dispatcher->async_dispatcher(),
                                       [&] { bus_device.reset(); }));

    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
      auto cancel_res = ep_client->CancelAll();
      ASSERT_TRUE(cancel_res.is_error());
      ASSERT_TRUE(cancel_res.error_value().is_domain_error());
      EXPECT_EQ(cancel_res.error_value().domain_error(), ZX_ERR_IO_NOT_PRESENT);
      ep_client = {};
      usb_client = {};
    }));
    driver_test().runtime().RunUntilIdle();
  }
}

TEST_F(XhciMmioHarness, HubDeviceRemovedWhenBusInvalidDoesNotAssert) {
  // 1. Connect a simulated USB hub on root hub port 1.
  FakeUsbDevice hub_dev = ConnectDevice(1, USB_SPEED_HIGH);
  const uint32_t hub_id = hub_dev.device_id;
  auto hub_state = driver_test().driver()->GetDeviceState(hub_id);
  ASSERT_TRUE(hub_state);
  EXPECT_EQ(driver_test().driver()->UsbHciHubDeviceRemoved(hub_id, 1), ZX_OK);

  // 2. Connect a child device on root hub port 2 (to allocate device state and slot).
  FakeUsbDevice child_dev = ConnectDevice(2, USB_SPEED_FULL);
  const uint32_t child_device_id = child_dev.device_id;
  auto child_state = driver_test().driver()->GetDeviceState(child_device_id);
  ASSERT_TRUE(child_state);

  // 3. Configure the hub state so child_device_id is attached downstream of hub port 1.
  HubInfo hub_info = {};
  hub_info.port_to_device[0] = static_cast<uint8_t>(child_device_id);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    hub_state->GetHubLocked() = hub_info;
  }

  // 4. Simulate bus unbind prior to xhci shutdown, clearing the bus interface
  // (matching DFv2 bottom-up unbind where usb-bus clears the bus interface).
  driver_test().driver()->UsbHciSetBusInterface(nullptr);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending_cmds = driver_test().driver()->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending_cmds) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  // 5. Invoke UsbHciHubDeviceRemoved for the child device on hub port 1.
  // Without the fix, ZX_ASSERT(bus_.is_valid()) fails fatally because bus_ was cleared.
  // With the fix, UsbHciHubDeviceRemoved returns ZX_OK safely.
  EXPECT_OK(driver_test().driver()->UsbHciHubDeviceRemoved(hub_id, 1));
}

class Xhci64SlotMmioHarness : public XhciHarness {
 protected:
  void SetUp() override {
    bus_.set_max_slots(64);
    driver_test().RunInEnvironmentTypeContext(
        [](XhciTestEnvironment& env) { env.fake_device().set_max_slots(64); });
    ASSERT_TRUE(driver_test()
                    .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                      xhci_config::Config fake_config;
                      fake_config.enable_suspend() = false;
                      args.config(fake_config.ToVmo());
                    })
                    .is_ok());
    ASSERT_OK(driver_test().driver()->TestInit(this));
    driver_test().driver()->UsbHciSetBusInterface(bus_.proto());
  }

  void TearDown() override { ASSERT_TRUE(driver_test().StopDriver().is_ok()); }
};

TEST_F(Xhci64SlotMmioHarness, RepeatedHotplugOn64SlotControllerDisablesSlotBeforeUnpinningDma) {
  UsbXhci* xhci = driver_test().driver();
  ASSERT_EQ(GetMaxDeviceCount(), 66UL);

  const zx_paddr_t dcbaa_phys = driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
      [](XhciTestEnvironment& env) { return env.fake_device().dcbaa_phys(); });
  ASSERT_NE(dcbaa_phys, 0u);
  auto* dcbaa = static_cast<uint64_t*>(
      reinterpret_cast<FakeVMO*>(dcbaa_phys)->virt);  // NOLINT(performance-no-int-to-ptr)

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);

  uint64_t hw_enabled_slots = 0;
  size_t disable_slot_count = 0;
  std::atomic<bool> expect_inflight_dma{false};
  std::atomic<bool> saw_disable_before_unpin{false};
  libsync::Completion inflight_req_completed;

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending_cmds = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending_cmds) {
        if (cmd.trb && Control::FromTRB(cmd.trb).Type() == Control::DisableSlot) {
          const uint8_t disabled_slot =
              static_cast<uint8_t>(reinterpret_cast<DisableSlot*>(cmd.trb)->slot());
          EXPECT_GE(disabled_slot, 1u);
          EXPECT_LE(disabled_slot, 64u);
          const uint64_t slot_bit = uint64_t{1} << (disabled_slot - 1u);
          EXPECT_NE(hw_enabled_slots & slot_bit, 0u);
          if (expect_inflight_dma.load(std::memory_order_acquire)) {
            EXPECT_FALSE(inflight_req_completed.signaled());
            saw_disable_before_unpin.store(!inflight_req_completed.signaled(),
                                           std::memory_order_release);
          }
          hw_enabled_slots &= ~slot_bit;
          FreeSlot(disabled_slot);
          ++disable_slot_count;
        }
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  // Attach an external hub on root hub port 2 so we exercise both root-hub DeviceOffline and
  // downstream-hub UsbHciHubDeviceRemoved across >64 hotplug cycles.
  FakeUsbDevice hub_dev = ConnectDevice(2, USB_SPEED_HIGH);
  const uint32_t hub_id = hub_dev.device_id;
  const uint8_t hub_slot = static_cast<uint8_t>(hub_id + 1);
  ASSERT_GE(hub_slot, 1u);
  ASSERT_LE(hub_slot, 64u);
  hw_enabled_slots |= (uint64_t{1} << (hub_slot - 1u));
  auto hub_state = xhci->GetDeviceState(hub_id);
  ASSERT_TRUE(hub_state);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    HubInfo hub_info = {};
    hub_info.hub_state = hub_state;
    hub_info.speed = USB_SPEED_HIGH;
    hub_info.hub_speed = USB_SPEED_HIGH;
    hub_info.rh_port = 2;
    hub_state->GetHubLocked() = hub_info;
  }

  constexpr size_t kHotplugCycles = 100;
  for (size_t cycle = 0; cycle < kHotplugCycles; ++cycle) {
    const bool via_hub = (cycle % 2) == 1;
    uint32_t dev_id = 0;
    uint8_t slot = 0;

    if (!via_hub) {
      FakeUsbDevice dev = ConnectDevice(1, USB_SPEED_SUPER);
      dev_id = dev.device_id;
      slot = static_cast<uint8_t>(dev_id + 1);
    } else {
      slot = AllocateSlot();
      ASSERT_GE(slot, 1u);
      ASSERT_LE(slot, 64u);
      dev_id = slot - 1;
      HubInfo child_hub_info = {};
      child_hub_info.hub_state = hub_state;
      child_hub_info.speed = USB_SPEED_HIGH;
      child_hub_info.hub_speed = USB_SPEED_HIGH;
      child_hub_info.rh_port = 2;
      child_hub_info.parent_port_number = 1;
      xhci->SetDeviceInformation(slot, 1, child_hub_info);
      xhci->ScheduleTask(0, xhci->AddressDeviceCommand(slot, 1, child_hub_info, true));
      xhci->RunUntilIdle();
      bus_.reset();
      ASSERT_OK(xhci->DeviceOnline(slot, 1, USB_SPEED_HIGH));
      EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait(); }));
    }

    ASSERT_GE(slot, 1u);
    ASSERT_LE(slot, 64u);
    const uint64_t slot_bit = uint64_t{1} << (slot - 1u);
    ASSERT_EQ(hw_enabled_slots & slot_bit, 0u) << "Hardware slot leaked on cycle " << cycle;
    hw_enabled_slots |= slot_bit;
    EXPECT_NE(dcbaa[slot], 0u);

    EnableEndpoint(dev_id, 1, false);
    xhci->RunUntilIdle();

    inflight_req_completed.Reset();
    saw_disable_before_unpin.store(false, std::memory_order_release);
    zx_status_t completion_status = ZX_OK;

    std::optional<usb::Request<void>> req;
    ASSERT_OK(usb::Request<void>::Alloc(&req, zx_system_get_page_size(), 0x01,
                                        xhci->UsbHciGetRequestSize()));
    req->request()->header.device_id = dev_id;
    req->request()->header.ep_address = 0x01;
    req->request()->header.length = zx_system_get_page_size();

    struct CallbackCtx {
      zx_status_t* status;
      libsync::Completion* done;
      size_t parent_req_size;
    } cb_ctx{
        .status = &completion_status,
        .done = &inflight_req_completed,
        .parent_req_size = xhci->UsbHciGetRequestSize(),
    };
    usb_request_complete_callback_t complete_cb = {
        .callback =
            [](void* ctx, usb_request_t* r) {
              auto* state = static_cast<CallbackCtx*>(ctx);
              *state->status = r->response.status;
              usb::Request<void> owned(r, state->parent_req_size);
              state->done->Signal();
            },
        .ctx = &cb_ctx,
    };
    xhci->UsbHciRequestQueue(req->take(), &complete_cb);
    ASSERT_FALSE(inflight_req_completed.signaled());

    expect_inflight_dma.store(true, std::memory_order_release);
    if (!via_hub) {
      RunDeviceOffline(slot);
      EXPECT_EQ(xhci->GetPortState(1).slot_id, 0u);
    } else {
      EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
          [&] { EXPECT_OK(xhci->UsbHciHubDeviceRemoved(hub_id, 1)); }));
      {
        fbl::AutoLock _(&hub_state->transaction_lock());
        EXPECT_EQ(hub_state->GetHubLocked()->port_to_device[0], std::nullopt);
      }
    }
    expect_inflight_dma.store(false, std::memory_order_release);

    EXPECT_TRUE(saw_disable_before_unpin.load(std::memory_order_acquire))
        << "Slot was not disabled prior to unpinning in-flight DMA on cycle " << cycle;
    EXPECT_OK(inflight_req_completed.Wait(zx::sec(5)));
    EXPECT_EQ(completion_status, ZX_ERR_IO_NOT_PRESENT);
    EXPECT_EQ(hw_enabled_slots & slot_bit, 0u);
    EXPECT_EQ(dcbaa[slot], 0u);
    EXPECT_FALSE(xhci->GetDeviceState(dev_id));
  }

  EXPECT_EQ(disable_slot_count, kHotplugCycles);
  EXPECT_EQ(hw_enabled_slots, uint64_t{1} << (hub_slot - 1u));

  // Finally unplug the root-hub-attached hub itself and verify its slot and PortState are freed.
  RunDeviceOffline(hub_slot);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    EXPECT_FALSE(hub_state->GetHubLocked().has_value());
  }
  EXPECT_EQ(xhci->GetPortState(2).slot_id, 0u);
  EXPECT_EQ(dcbaa[hub_slot], 0u);
  EXPECT_EQ(hw_enabled_slots, 0u);
  EXPECT_EQ(disable_slot_count, kHotplugCycles + 1);
}

TEST_F(Xhci64SlotMmioHarness, UnpluggingExternalHubDisablesDownstreamChildSlotsBeforeUnpin) {
  UsbXhci* xhci = driver_test().driver();
  ASSERT_EQ(GetMaxDeviceCount(), 66UL);

  const zx_paddr_t dcbaa_phys = driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
      [](XhciTestEnvironment& env) { return env.fake_device().dcbaa_phys(); });
  ASSERT_NE(dcbaa_phys, 0u);
  auto* dcbaa = static_cast<uint64_t*>(
      reinterpret_cast<FakeVMO*>(dcbaa_phys)->virt);  // NOLINT(performance-no-int-to-ptr)

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);

  uint64_t hw_enabled_slots = 0;
  bool saw_child_disable_before_unpin = false;
  bool saw_hub_disable = false;
  std::atomic<bool> expect_inflight_dma{false};
  libsync::Completion inflight_req_completed;

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending_cmds = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending_cmds) {
        if (cmd.trb && Control::FromTRB(cmd.trb).Type() == Control::DisableSlot) {
          const uint8_t disabled_slot =
              static_cast<uint8_t>(reinterpret_cast<DisableSlot*>(cmd.trb)->slot());
          if (disabled_slot == 2) {  // child_slot is 2
            if (expect_inflight_dma.load(std::memory_order_acquire)) {
              saw_child_disable_before_unpin = !inflight_req_completed.signaled();
            }
          }
          if (disabled_slot == 1) {  // hub_slot is 1
            saw_hub_disable = true;
          }
          hw_enabled_slots &= ~(uint64_t{1} << (disabled_slot - 1u));
          success_event.set_SlotID(disabled_slot);
          FreeSlot(disabled_slot);
        }
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  FakeUsbDevice hub_dev = ConnectDevice(2, USB_SPEED_HIGH);
  const uint32_t hub_id = hub_dev.device_id;
  const uint8_t hub_slot = static_cast<uint8_t>(hub_id + 1);
  ASSERT_EQ(hub_slot, 1u);
  hw_enabled_slots |= (uint64_t{1} << (hub_slot - 1u));
  auto hub_state = xhci->GetDeviceState(hub_id);
  ASSERT_TRUE(hub_state);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    HubInfo hub_info = {};
    hub_info.hub_state = hub_state;
    hub_info.speed = USB_SPEED_HIGH;
    hub_info.hub_speed = USB_SPEED_HIGH;
    hub_info.rh_port = 2;
    hub_state->GetHubLocked() = hub_info;
  }

  uint8_t child_slot = AllocateSlot();
  ASSERT_EQ(child_slot, 2u);
  uint32_t child_id = child_slot - 1;
  HubInfo child_hub_info = {};
  child_hub_info.hub_state = hub_state;
  child_hub_info.speed = USB_SPEED_HIGH;
  child_hub_info.hub_speed = USB_SPEED_HIGH;
  child_hub_info.rh_port = 2;
  child_hub_info.parent_port_number = 1;
  xhci->SetDeviceInformation(child_slot, 1, child_hub_info);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(child_slot, 1, child_hub_info, true));
  xhci->RunUntilIdle();
  bus_.reset();
  ASSERT_OK(xhci->DeviceOnline(child_slot, 1, USB_SPEED_HIGH));
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait(); }));
  hw_enabled_slots |= (uint64_t{1} << (child_slot - 1u));

  EnableEndpoint(child_id, 1, false);
  xhci->RunUntilIdle();

  zx_status_t completion_status = ZX_OK;
  std::optional<usb::Request<void>> req;
  ASSERT_OK(usb::Request<void>::Alloc(&req, zx_system_get_page_size(), 0x01,
                                      xhci->UsbHciGetRequestSize()));
  req->request()->header.device_id = child_id;
  req->request()->header.ep_address = 0x01;
  req->request()->header.length = zx_system_get_page_size();

  struct CallbackCtx {
    zx_status_t* status;
    libsync::Completion* done;
    size_t parent_req_size;
  } cb_ctx{
      .status = &completion_status,
      .done = &inflight_req_completed,
      .parent_req_size = xhci->UsbHciGetRequestSize(),
  };
  usb_request_complete_callback_t complete_cb = {
      .callback =
          [](void* ctx, usb_request_t* r) {
            auto* state = static_cast<CallbackCtx*>(ctx);
            *state->status = r->response.status;
            usb::Request<void> owned(r, state->parent_req_size);
            state->done->Signal();
          },
      .ctx = &cb_ctx,
  };
  xhci->UsbHciRequestQueue(req->take(), &complete_cb);
  ASSERT_FALSE(inflight_req_completed.signaled());

  expect_inflight_dma.store(true, std::memory_order_release);
  RunDeviceOffline(hub_slot);
  EXPECT_EQ(bus_.devices().count(hub_id), 0u);
  EXPECT_EQ(bus_.devices().count(child_id), 0u);

  EXPECT_TRUE(saw_child_disable_before_unpin)
      << "Child slot was not disabled prior to unpinning in-flight DMA on hub unplug";
  EXPECT_TRUE(saw_hub_disable) << "Hub slot was not disabled on hub unplug";

  EXPECT_OK(inflight_req_completed.Wait(zx::sec(5)));
  EXPECT_EQ(completion_status, ZX_ERR_IO_NOT_PRESENT);

  EXPECT_EQ(xhci->GetPortState(2).slot_id, 0u);
  EXPECT_EQ(dcbaa[hub_slot], 0u);
  EXPECT_EQ(dcbaa[child_slot], 0u);
  EXPECT_FALSE(xhci->GetDeviceState(hub_id));
  EXPECT_FALSE(xhci->GetDeviceState(child_id));
  EXPECT_EQ(hw_enabled_slots, 0u);
}

TEST_F(XhciMmioHarness, IrqCountClampedToPDevDeviceInfo) {
  // Verify that UsbXhci clamps irq_count_ to the PDev IRQ count instead of allocating
  // excess interrupters beyond what the platform device provides.
  EXPECT_EQ(driver_test().driver()->irq_count(), 1u);
}

TEST_F(XhciMmioHarness, ConnectToEndpointBoundsCheck) {
  zx::result hci_client_end =
      driver_test().Connect<fuchsia_hardware_usb_hci::UsbHciService::Device>();
  ASSERT_OK(hci_client_end);

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    fidl::WireSyncClient<fuchsia_hardware_usb_hci::UsbHci> hci_client(std::move(*hci_client_end));

    // 1. Invalid device_id (out of bounds).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(1000, 1U | 0x80U, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }

    // 2. Invalid endpoint address: ep 0 (EP0 is control, not connected via ConnectToEndpoint).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(0, 0, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }

    // 3. Invalid endpoint address: 0x80 (ep 0 IN).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(0, 0x80, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }

    // 4. Invalid endpoint address: 0x10 (out of USB endpoint index range).
    {
      auto [ep_client_end, ep_server_end] =
          fidl::Endpoints<fuchsia_hardware_usb_endpoint::Endpoint>::Create();
      auto res = hci_client->ConnectToEndpoint(0, 0x10, std::move(ep_server_end));
      ASSERT_TRUE(res.ok());
      ASSERT_TRUE(res->is_error());
      EXPECT_EQ(res->error_value(), ZX_ERR_NOT_FOUND);
    }
  }));
}

TEST_F(XhciMmioHarness, RequestQueueBoundsCheck) {
  ConnectDevice(1, USB_SPEED_HIGH);

  // 1. Invalid device_id
  {
    bool invoked = false;
    std::optional<TestRequest> request;
    AllocateRequest(&request, 0, 64, 1, [&](TestRequest req) {
      invoked = true;
      EXPECT_EQ(req.request()->response.status, ZX_ERR_INVALID_ARGS);
    });
    request->request()->header.device_id = 1000;
    RequestQueue(std::move(*request));
    EXPECT_TRUE(invoked);
  }

  // 2. Invalid ep_address (0x80)
  {
    bool invoked = false;
    std::optional<TestRequest> request;
    AllocateRequest(&request, 0, 64, 0x80, [&](TestRequest req) {
      invoked = true;
      EXPECT_EQ(req.request()->response.status, ZX_ERR_INVALID_ARGS);
    });
    request->request()->header.device_id = 0;
    RequestQueue(std::move(*request));
    EXPECT_TRUE(invoked);
  }

  // 3. Invalid ep_address (0x10)
  {
    bool invoked = false;
    std::optional<TestRequest> request;
    AllocateRequest(&request, 0, 64, 0x10, [&](TestRequest req) {
      invoked = true;
      EXPECT_EQ(req.request()->response.status, ZX_ERR_INVALID_ARGS);
    });
    request->request()->header.device_id = 0;
    RequestQueue(std::move(*request));
    EXPECT_TRUE(invoked);
  }
}

TEST_F(XhciMmioHarness, ResetEndpointBoundsCheck) {
  ConnectDevice(1, USB_SPEED_HIGH);
  EXPECT_EQ(driver_test().driver()->UsbHciResetEndpoint(1000, 1), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(driver_test().driver()->UsbHciResetEndpoint(0, 0x80), ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(driver_test().driver()->UsbHciResetEndpoint(0, 0x10), ZX_ERR_INVALID_ARGS);
}

TEST_F(XhciMmioHarness, CancelAllBoundsAndEP0) {
  ConnectDevice(1, USB_SPEED_HIGH);

  // 1. Invalid device_id
  EXPECT_EQ(driver_test().driver()->UsbHciCancelAll(1000, 1), ZX_ERR_INVALID_ARGS);

  // 2. Invalid ep_address (0x80)
  EXPECT_EQ(driver_test().driver()->UsbHciCancelAll(0, 0x80), ZX_ERR_INVALID_ARGS);

  // 3. Invalid ep_address (0x10)
  EXPECT_EQ(driver_test().driver()->UsbHciCancelAll(0, 0x10), ZX_ERR_INVALID_ARGS);

  // 4. CancelAll on EP0 (ep_address == 0)
  // Complete AddressDeviceCommand
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  EXPECT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  EXPECT_OK(CompleteCommand(cr, &success_event));

  // Now execute CancelAll on EP0 (ep_address == 0).
  // This sends StopEndpoint for DCI 1 (ENDPOINT == 1).
  bool got_stop_endpoint = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      Control control = Control::FromTRB(cr);
      switch (control.Type()) {
        case Control::StopEndpointCommand: {
          auto stop = reinterpret_cast<StopEndpoint*>(cr);
          EXPECT_EQ(stop->ENDPOINT(), 1u);
          EXPECT_EQ(stop->SLOT(), 1u);
          got_stop_endpoint = true;
          EXPECT_OK(CompleteCommand(cr, &success_event));
        } break;
      }
    }
  });

  zx_status_t status = driver_test().driver()->UsbHciCancelAll(0, 0);
  EXPECT_TRUE(got_stop_endpoint);
  EXPECT_OK(status);
}

TEST_F(XhciMmioHarness, DestructorCompletesPendingCommandWithIoNotPresent) {
  ConnectDevice(1, USB_SPEED_HIGH);
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  ASSERT_OK(CompleteCommand(cr, &success_event));

  UsbXhci* xhci = driver_test().driver();
  auto context = xhci->GetCommandRing()->AllocateContext();
  ASSERT_TRUE(context);
  fpromise::bridge<TRB*, zx_status_t> bridge;
  context->completer = std::move(bridge.completer);

  bool completed = false;
  zx_status_t completion_error = ZX_OK;
  xhci->ScheduleTask(0, bridge.consumer.promise()
                            .then([&](fpromise::result<TRB*, zx_status_t>& result)
                                      -> fpromise::result<void, zx_status_t> {
                              completed = true;
                              if (result.is_error()) {
                                completion_error = result.error();
                              }
                              return fpromise::ok();
                            })
                            .box());

  TRB cmd{};
  ASSERT_OK(xhci->GetCommandRing()->AddTRB(cmd, std::move(context)));

  // Also queue an EP0 transfer request whose completion wakes an interrupter task that queues a
  // follow-up command (modeling EnumerateDevice's GetDeviceDescriptor failure scheduling
  // DisableSlotCommand during teardown).
  fpromise::bridge<void, void> ep0_bridge;
  bool followup_cmd_completed = false;
  zx_status_t followup_cmd_error = ZX_OK;
  xhci->ScheduleTask(
      0, ep0_bridge.consumer.promise()
             .then([&](fpromise::result<void, void>&) -> fpromise::result<void, zx_status_t> {
               auto followup_ctx = xhci->GetCommandRing()->AllocateContext();
               EXPECT_TRUE(followup_ctx);
               fpromise::bridge<TRB*, zx_status_t> followup_bridge;
               followup_ctx->completer = std::move(followup_bridge.completer);
               xhci->ScheduleTask(0, followup_bridge.consumer.promise()
                                         .then([&](fpromise::result<TRB*, zx_status_t>& res)
                                                   -> fpromise::result<void, zx_status_t> {
                                           followup_cmd_completed = true;
                                           if (res.is_error()) {
                                             followup_cmd_error = res.error();
                                           }
                                           return fpromise::ok();
                                         })
                                         .box());
               TRB followup_trb{};
               EXPECT_OK(xhci->GetCommandRing()->AddTRB(followup_trb, std::move(followup_ctx)));
               return fpromise::ok();
             })
             .box());

  bool ep0_req_completed = false;
  std::optional<TestRequest> ep0_req;
  ASSERT_OK(AllocateRequest(&ep0_req, 0, 8, 0, [&](TestRequest req) {
    ep0_req_completed = true;
    EXPECT_EQ(req.request()->response.status, ZX_ERR_IO_NOT_PRESENT);
    ep0_bridge.completer.complete_ok();
  }));
  RequestQueue(std::move(*ep0_req));

  // Verify that StopDriver() and ShutdownAndDestroyDriver() drain the pending command with
  // ZX_ERR_IO_NOT_PRESENT and run the stopped interrupter's executor after both command ring and
  // transfer ring drains so follow-up commands scheduled by transfer completions also drain.
  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());
  driver_test().ShutdownAndDestroyDriver();
  EXPECT_TRUE(completed);
  EXPECT_EQ(completion_error, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(ep0_req_completed);
  EXPECT_TRUE(followup_cmd_completed);
  EXPECT_EQ(followup_cmd_error, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, DestructorDoesNotInstantiateUnusedEndpointsOrLeakConfiguredHub) {
  ConnectDevice(1, USB_SPEED_HIGH);
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  ASSERT_OK(CompleteCommand(cr, &success_event));

  UsbXhci* xhci = driver_test().driver();
  auto root_hub_state = xhci->GetDeviceState(0);
  ASSERT_TRUE(root_hub_state);

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      ASSERT_NE(cr, nullptr);
      EXPECT_OK(CompleteCommand(cr, &success_event));
    }
  });

  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 4;
  ASSERT_OK(xhci->UsbHciConfigureHub(0, USB_SPEED_HIGH, &hub_desc, true));

  // Set up a nested hub on slot 2 parented to root_hub_state.
  const uint8_t child_slot = AllocateSlot();
  ASSERT_EQ(child_slot, 2u);
  HubInfo parent_hub_info(root_hub_state, 0, USB_SPEED_HIGH, true);
  parent_hub_info.speed = USB_SPEED_HIGH;
  parent_hub_info.rh_port = 1;
  parent_hub_info.parent_port_number = 1;
  parent_hub_info.route_string = 1;
  xhci->SetDeviceInformation(child_slot, 1, parent_hub_info);
  (void)xhci->AddressDeviceCommand(child_slot, 1, parent_hub_info, true);

  auto child_hub_state = xhci->GetDeviceState(1);
  ASSERT_TRUE(child_hub_state);
  ASSERT_OK(xhci->UsbHciConfigureHub(1, USB_SPEED_HIGH, &hub_desc, false));

  {
    fbl::AutoLock _(&root_hub_state->transaction_lock());
    ASSERT_TRUE(root_hub_state->GetHubLocked().has_value());
    EXPECT_EQ(root_hub_state->GetHubLocked()->hub_state.get(), nullptr);
    for (uint8_t i = 0; i < kMaxEndpoints; i++) {
      EXPECT_EQ(root_hub_state->TryGetEndpoint(i), nullptr);
    }
  }
  {
    fbl::AutoLock _(&child_hub_state->transaction_lock());
    ASSERT_TRUE(child_hub_state->GetHubLocked().has_value());
    EXPECT_EQ(child_hub_state->GetHubLocked()->hub_state.get(), root_hub_state.get());
    EXPECT_EQ(child_hub_state->GetHubLocked()->hub_depth, 1u);
    for (uint8_t i = 0; i < kMaxEndpoints; i++) {
      EXPECT_EQ(child_hub_state->TryGetEndpoint(i), nullptr);
    }
  }

  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());
  driver_test().ShutdownAndDestroyDriver();

  {
    fbl::AutoLock _(&root_hub_state->transaction_lock());
    EXPECT_FALSE(root_hub_state->GetHubLocked().has_value());
    for (uint8_t i = 0; i < kMaxEndpoints; i++) {
      EXPECT_EQ(root_hub_state->TryGetEndpoint(i), nullptr);
    }
  }
  {
    fbl::AutoLock _(&child_hub_state->transaction_lock());
    EXPECT_FALSE(child_hub_state->GetHubLocked().has_value());
    for (uint8_t i = 0; i < kMaxEndpoints; i++) {
      EXPECT_EQ(child_hub_state->TryGetEndpoint(i), nullptr);
    }
  }
}

TEST_F(XhciHarness, HciFinalizeZeroScratchpadBuffers) {
  driver_test().RunInEnvironmentTypeContext(
      [](XhciTestEnvironment& env) { env.fake_device().set_max_scratchpad_buffers(0, 0); });
  ASSERT_TRUE(driver_test()
                  .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                    xhci_config::Config fake_config;
                    fake_config.enable_suspend() = false;
                    args.config(fake_config.ToVmo());
                  })
                  .is_ok());
  ASSERT_OK(driver_test().driver()->TestInit(this));

  zx_paddr_t dcbaa_phys = driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
      [](XhciTestEnvironment& env) { return env.fake_device().dcbaa_phys(); });
  ASSERT_NE(dcbaa_phys, 0u);
  uint64_t* dcbaa_virt = ddk_fake::PhysToVirt<uint64_t*>(dcbaa_phys);
  EXPECT_EQ(dcbaa_virt[0], 0u);

  ASSERT_TRUE(driver_test().StopDriver().is_ok());
}

TEST_F(XhciHarness, HciFinalizeMaxSinglePageScratchpadBuffers) {
  // Exercise the exact maximum count fitting in a single 4 KiB Scratchpad Buffer Array page
  // (512 * 8 == 4096 bytes when zx_system_get_page_size() == 4096). Previously, an erroneous +1
  // inflated 512 to 513 and failed HciFinalize() with ZX_ERR_NOT_SUPPORTED.
  const size_t max_entries = zx_system_get_page_size() / sizeof(uint64_t);
  if (max_entries > 1023) {
    GTEST_SKIP() << "System page size exceeds 10-bit HCSPARAMS2 scratchpad count range";
  }
  const auto low = static_cast<uint8_t>(max_entries & 0x1fu);
  const auto high = static_cast<uint8_t>(max_entries >> 5u);

  driver_test().RunInEnvironmentTypeContext([low, high](XhciTestEnvironment& env) {
    env.fake_device().set_max_scratchpad_buffers(low, high);
  });
  ASSERT_TRUE(driver_test()
                  .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                    xhci_config::Config fake_config;
                    fake_config.enable_suspend() = false;
                    args.config(fake_config.ToVmo());
                  })
                  .is_ok());
  ASSERT_OK(driver_test().driver()->TestInit(this));

  zx_paddr_t dcbaa_phys = driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
      [](XhciTestEnvironment& env) { return env.fake_device().dcbaa_phys(); });
  ASSERT_NE(dcbaa_phys, 0u);
  uint64_t* dcbaa_virt = ddk_fake::PhysToVirt<uint64_t*>(dcbaa_phys);
  ASSERT_NE(dcbaa_virt[0], 0u);

  uint64_t* scratchpad_array = ddk_fake::PhysToVirt<uint64_t*>(dcbaa_virt[0]);
  EXPECT_NE(scratchpad_array[0], 0u);
  EXPECT_NE(scratchpad_array[max_entries - 1], 0u);

  ASSERT_TRUE(driver_test().StopDriver().is_ok());
}

TEST_F(XhciHarness, HciFinalizeScratchpadBufferCountExceedsPageReturnsNotSupported) {
  const size_t over_entries = (zx_system_get_page_size() / sizeof(uint64_t)) + 1;
  if (over_entries > 1023) {
    GTEST_SKIP() << "System page size exceeds 10-bit HCSPARAMS2 scratchpad count range";
  }
  const auto low = static_cast<uint8_t>(over_entries & 0x1fu);
  const auto high = static_cast<uint8_t>(over_entries >> 5u);

  driver_test().RunInEnvironmentTypeContext([low, high](XhciTestEnvironment& env) {
    env.fake_device().set_max_scratchpad_buffers(low, high);
  });
  ASSERT_TRUE(driver_test()
                  .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                    xhci_config::Config fake_config;
                    fake_config.enable_suspend() = false;
                    args.config(fake_config.ToVmo());
                  })
                  .is_ok());
  EXPECT_EQ(driver_test().driver()->TestInit(this).status_value(), ZX_ERR_NOT_SUPPORTED);

  ASSERT_TRUE(driver_test().StopDriver().is_ok());
}

TEST_F(XhciHarness, HciFinalizeHighScratchpadBufferCount) {
  // Exercise an odd multi-field count: MAX_SCRATCHPAD_BUFFERS_LOW (3) |
  // (MAX_SCRATCHPAD_BUFFERS_HIGH (2) << 5) == 67, verifying that bit 0 does not mask an erroneous
  // +1 and all 67 entries (0..66) are populated.
  constexpr uint8_t kLow = 3;
  constexpr uint8_t kHigh = 2;
  constexpr size_t kExpectedCount = static_cast<size_t>(kLow) | (static_cast<size_t>(kHigh) << 5u);
  static_assert(kExpectedCount == 67);

  driver_test().RunInEnvironmentTypeContext(
      [](XhciTestEnvironment& env) { env.fake_device().set_max_scratchpad_buffers(kLow, kHigh); });
  ASSERT_TRUE(driver_test()
                  .StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
                    xhci_config::Config fake_config;
                    fake_config.enable_suspend() = false;
                    args.config(fake_config.ToVmo());
                  })
                  .is_ok());
  ASSERT_OK(driver_test().driver()->TestInit(this));

  zx_paddr_t dcbaa_phys = driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
      [](XhciTestEnvironment& env) { return env.fake_device().dcbaa_phys(); });
  ASSERT_NE(dcbaa_phys, 0u);
  uint64_t* dcbaa_virt = ddk_fake::PhysToVirt<uint64_t*>(dcbaa_phys);
  ASSERT_NE(dcbaa_virt[0], 0u);

  uint64_t* scratchpad_array = ddk_fake::PhysToVirt<uint64_t*>(dcbaa_virt[0]);
  for (size_t i = 0; i < kExpectedCount; ++i) {
    EXPECT_NE(scratchpad_array[i], 0u) << "scratchpad_array[" << i << "] was left uninitialized";
  }
  EXPECT_EQ(scratchpad_array[kExpectedCount], 0u);

  ASSERT_TRUE(driver_test().StopDriver().is_ok());
}

TEST_F(XhciMmioHarness, ConfigureHubWithActiveEndpointAndDisableEndpoint) {
  usb_hub_descriptor_t hs_hub_desc = {};
  hs_hub_desc.b_nbr_ports = 4;
  hs_hub_desc.w_hub_characteristics = htole16(0x0040);  // TTT = 2

  // Invalid device_id, null descriptor, and unallocated slot checks.
  EXPECT_EQ(driver_test().driver()->UsbHciConfigureHub(1000, USB_SPEED_HIGH, &hs_hub_desc, true),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(driver_test().driver()->UsbHciConfigureHub(0, USB_SPEED_HIGH, nullptr, true),
            ZX_ERR_INVALID_ARGS);
  EXPECT_EQ(driver_test().driver()->UsbHciConfigureHub(0, USB_SPEED_HIGH, &hs_hub_desc, true),
            ZX_ERR_IO_NOT_PRESENT);

  ConnectDevice(1, USB_SPEED_FULL);
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  CommandCompletionEvent param_error_event;
  param_error_event.set_CompletionCode(CommandCompletionEvent::ParameterError);

  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  ASSERT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  ASSERT_OK(CompleteCommand(cr, &success_event));

  CommandCompletionEvent* next_event = &success_event;
  size_t configure_cmd_count = 0;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      ASSERT_NE(cr, nullptr);
      if (Control::FromTRB(cr).Type() == Control::ConfigureEndpointCommand) {
        ++configure_cmd_count;
      }
      EXPECT_OK(CompleteCommand(cr, next_event));
    }
  });

  // Enable interrupt IN endpoint 0x81 first so CONTEXT_ENTRIES becomes 3.
  usb_endpoint_descriptor_t int_ep_desc = {};
  int_ep_desc.b_length = sizeof(usb_endpoint_descriptor_t);
  int_ep_desc.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint);
  int_ep_desc.b_endpoint_address = 0x81;
  int_ep_desc.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt);
  int_ep_desc.w_max_packet_size = htole16(8);
  int_ep_desc.b_interval = 4;

  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &int_ep_desc, nullptr, true));
  ASSERT_EQ(configure_cmd_count, 1u);

  // Verify both command failure rollback (preserving CONTEXT_ENTRIES) and High-Speed / Full-Speed
  // TTT handling on success.
  usb_hub_descriptor_t fs_hub_desc = {};
  fs_hub_desc.b_nbr_ports = 7;
  fs_hub_desc.w_hub_characteristics = htole16(0x0060);

  auto state = driver_test().driver()->GetDeviceState(0);
  ASSERT_TRUE(state);
  const size_t slot_size = driver_test().driver()->slot_size_bytes();

  next_event = &param_error_event;
  EXPECT_EQ(driver_test().driver()->UsbHciConfigureHub(0, USB_SPEED_FULL, &fs_hub_desc, false),
            ZX_ERR_IO);
  EXPECT_EQ(configure_cmd_count, 2u);
  {
    fbl::AutoLock l(&state->transaction_lock());
    EXPECT_FALSE(state->GetHubLocked().has_value());
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    EXPECT_EQ(control[0], 0u);
    EXPECT_EQ(control[1], 0u);
    auto* slot_ctx =
        reinterpret_cast<SlotContext*>(reinterpret_cast<uint8_t*>(control) + slot_size);
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 3u);
    EXPECT_EQ(slot_ctx->HUB(), 0u);
    EXPECT_EQ(slot_ctx->PORT_COUNT(), 0u);
  }

  next_event = &success_event;
  EXPECT_OK(driver_test().driver()->UsbHciConfigureHub(0, USB_SPEED_HIGH, &hs_hub_desc, true));
  EXPECT_EQ(configure_cmd_count, 3u);
  {
    fbl::AutoLock l(&state->transaction_lock());
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    auto* slot_ctx =
        reinterpret_cast<SlotContext*>(reinterpret_cast<uint8_t*>(control) + slot_size);
    EXPECT_EQ(slot_ctx->HUB(), 1u);
    EXPECT_EQ(slot_ctx->PORT_COUNT(), 4u);
    EXPECT_EQ(slot_ctx->TTT(), 2u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
    EXPECT_EQ(slot_ctx->SPEED(), USB_SPEED_HIGH);
  }

  EXPECT_OK(driver_test().driver()->UsbHciConfigureHub(0, USB_SPEED_FULL, &fs_hub_desc, false));
  EXPECT_EQ(configure_cmd_count, 4u);

  {
    fbl::AutoLock l(&state->transaction_lock());
    ASSERT_TRUE(state->GetHubLocked().has_value());
    EXPECT_EQ(state->GetHubLocked()->hub_state.get(), nullptr);
    EXPECT_TRUE(state->GetHubLocked()->hub_configured);
    EXPECT_EQ(state->GetHubLocked()->hub_depth, 0u);
    for (const auto& dev_id : state->GetHubLocked()->port_to_device) {
      EXPECT_EQ(dev_id, std::nullopt);
    }
    state->GetHubLocked()->port_to_device[2] = 5;

    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    EXPECT_EQ(control[0], 0u);
    EXPECT_EQ(control[1], 1u);
    auto* slot_ctx =
        reinterpret_cast<SlotContext*>(reinterpret_cast<uint8_t*>(control) + slot_size);
    EXPECT_EQ(slot_ctx->HUB(), 1u);
    EXPECT_EQ(slot_ctx->PORT_COUNT(), 7u);
    EXPECT_EQ(slot_ctx->TTT(), 0u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  // Reconfiguring an already-configured hub must preserve hub_depth and port_to_device without
  // creating a self-referencing hub_state RefPtr cycle.
  EXPECT_OK(driver_test().driver()->UsbHciConfigureHub(0, USB_SPEED_FULL, &fs_hub_desc, false));
  EXPECT_EQ(configure_cmd_count, 5u);
  {
    fbl::AutoLock l(&state->transaction_lock());
    ASSERT_TRUE(state->GetHubLocked().has_value());
    EXPECT_EQ(state->GetHubLocked()->hub_state.get(), nullptr);
    EXPECT_TRUE(state->GetHubLocked()->hub_configured);
    EXPECT_EQ(state->GetHubLocked()->hub_depth, 0u);
    EXPECT_EQ(state->GetHubLocked()->port_to_device[2], 5u);
    EXPECT_EQ(state->GetHubLocked()->port_to_device[0], std::nullopt);
  }

  // Enable a second endpoint (0x82, index 4 -> DCI 5) so CONTEXT_ENTRIES becomes 5.
  usb_endpoint_descriptor_t second_ep_desc = int_ep_desc;
  second_ep_desc.b_endpoint_address = 0x82;
  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &second_ep_desc, nullptr, true));
  EXPECT_EQ(configure_cmd_count, 6u);

  // Disabling 0x82 (DCI 5) while 0x81 (DCI 3) remains active must update CONTEXT_ENTRIES to 3.
  EXPECT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &second_ep_desc, nullptr, false));
  EXPECT_EQ(configure_cmd_count, 7u);
  {
    fbl::AutoLock l(&state->transaction_lock());
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    EXPECT_EQ(control[0], 1u << 5u);
    EXPECT_EQ(control[1], 1u);
    auto* slot_ctx =
        reinterpret_cast<SlotContext*>(reinterpret_cast<uint8_t*>(control) + slot_size);
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 3u);
  }

  // Disabling the last remaining non-EP0 endpoint (0x81, DCI 3) must reset CONTEXT_ENTRIES to 1.
  EXPECT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &int_ep_desc, nullptr, false));
  EXPECT_EQ(configure_cmd_count, 8u);
  {
    fbl::AutoLock l(&state->transaction_lock());
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    EXPECT_EQ(control[0], 1u << 3u);
    EXPECT_EQ(control[1], 1u);
    auto* slot_ctx =
        reinterpret_cast<SlotContext*>(reinterpret_cast<uint8_t*>(control) + slot_size);
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 1u);
  }

  // Configuring a nested Tier-2 hub attached downstream of slot 1 must preserve its parent
  // hub_state (pointing to slot 1 rather than creating a self-reference), increment hub_depth to 1
  // once, and preserve both on subsequent reconfiguration.
  const uint8_t tier2_slot = AllocateSlot();
  ASSERT_EQ(tier2_slot, 2u);
  HubInfo tier2_parent(state, 0, USB_SPEED_FULL, false);
  tier2_parent.speed = USB_SPEED_FULL;
  tier2_parent.rh_port = 1;
  tier2_parent.parent_port_number = 1;
  tier2_parent.route_string = 1;
  driver_test().driver()->SetDeviceInformation(tier2_slot, 1, tier2_parent);
  (void)driver_test().driver()->AddressDeviceCommand(tier2_slot, 1, tier2_parent, true);

  EXPECT_OK(driver_test().driver()->UsbHciConfigureHub(1, USB_SPEED_FULL, &fs_hub_desc, false));
  auto tier2_state = driver_test().driver()->GetDeviceState(1);
  ASSERT_TRUE(tier2_state);
  {
    fbl::AutoLock l(&tier2_state->transaction_lock());
    ASSERT_TRUE(tier2_state->GetHubLocked().has_value());
    EXPECT_EQ(tier2_state->GetHubLocked()->hub_state.get(), state.get());
    EXPECT_TRUE(tier2_state->GetHubLocked()->hub_configured);
    EXPECT_EQ(tier2_state->GetHubLocked()->hub_depth, 1u);
    tier2_state->GetHubLocked()->port_to_device[0] = 9;
  }

  EXPECT_OK(driver_test().driver()->UsbHciConfigureHub(1, USB_SPEED_FULL, &fs_hub_desc, false));
  {
    fbl::AutoLock l(&tier2_state->transaction_lock());
    ASSERT_TRUE(tier2_state->GetHubLocked().has_value());
    EXPECT_EQ(tier2_state->GetHubLocked()->hub_state.get(), state.get());
    EXPECT_TRUE(tier2_state->GetHubLocked()->hub_configured);
    EXPECT_EQ(tier2_state->GetHubLocked()->hub_depth, 1u);
    EXPECT_EQ(tier2_state->GetHubLocked()->port_to_device[0], 9u);
  }

  // If the device disconnects (ResetHci clears hub_ and marks disconnecting_) while ConfigureHub's
  // ConfigureEndpointCommand is in flight and the command fails, rollback must not resurrect hub_
  // or its parent hub_state reference.
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      ASSERT_NE(cr, nullptr);
      if (Control::FromTRB(cr).Type() == Control::ConfigureEndpointCommand) {
        ++configure_cmd_count;
      }
      tier2_state->ResetHci();
      EXPECT_OK(CompleteCommand(cr, &param_error_event));
    }
  });
  EXPECT_EQ(driver_test().driver()->UsbHciConfigureHub(1, USB_SPEED_FULL, &fs_hub_desc, false),
            ZX_ERR_IO);
  {
    fbl::AutoLock l(&tier2_state->transaction_lock());
    EXPECT_TRUE(tier2_state->IsDisconnecting());
    EXPECT_FALSE(tier2_state->GetHubLocked().has_value());
  }
}

TEST_F(XhciMmioHarness, HighBandwidthAndSuperSpeedInterruptEsitPayload) {
  ConnectDevice(1, USB_SPEED_HIGH);
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);

  auto cr = crcr_next();
  ASSERT_NE(cr, nullptr);
  ASSERT_EQ(Control::FromTRB(cr).Type(), Control::AddressDeviceCommand);
  ASSERT_OK(CompleteCommand(cr, &success_event));

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      cr = FakeTRB::get(cr->next);
      ASSERT_NE(cr, nullptr);
      EXPECT_OK(CompleteCommand(cr, &success_event));
    }
  });

  // High-Speed high-bandwidth interrupt endpoint: bits 12:11 == 2 (3 transactions per microframe)
  // and 1024-byte max packet size -> MaxBurstSize == 2, MAX_ESIT_PAYLOAD_LOW == 3072.
  usb_endpoint_descriptor_t hs_int_ep = {};
  hs_int_ep.b_length = sizeof(usb_endpoint_descriptor_t);
  hs_int_ep.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint);
  hs_int_ep.b_endpoint_address = 0x81;
  hs_int_ep.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt);
  hs_int_ep.w_max_packet_size = htole16(static_cast<uint16_t>((2u << 11) | 1024u));
  hs_int_ep.b_interval = 1;

  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &hs_int_ep, nullptr, true));

  auto state = driver_test().driver()->GetDeviceState(0);
  ASSERT_TRUE(state);
  const size_t slot_size = driver_test().driver()->slot_size_bytes();
  const uint8_t ep_index = XhciEndpointIndex(hs_int_ep.b_endpoint_address);
  {
    fbl::AutoLock l(&state->transaction_lock());
    auto* control = static_cast<uint8_t*>(state->GetInputContext()->virt());
    auto* ep_ctx = reinterpret_cast<EndpointContext*>(control + (slot_size * (2 + ep_index)));
    EXPECT_EQ(ep_ctx->MAX_PACKET_SIZE(), 1024u);
    EXPECT_EQ(ep_ctx->MaxBurstSize(), 2u);
    EXPECT_EQ(ep_ctx->MAX_ESIT_PAYLOAD_LOW(), 3072u);
  }

  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &hs_int_ep, nullptr, false));

  // SuperSpeed interrupt endpoint with companion descriptor: b_max_burst == 3 and
  // w_bytes_per_interval == 2048 (distinct from 1024 * (3 + 1) == 4096) -> MaxBurstSize == 3,
  // MAX_ESIT_PAYLOAD_LOW == 2048.
  usb_endpoint_descriptor_t ss_int_ep = {};
  ss_int_ep.b_length = sizeof(usb_endpoint_descriptor_t);
  ss_int_ep.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint);
  ss_int_ep.b_endpoint_address = 0x82;
  ss_int_ep.bm_attributes = fidl::ToUnderlying(fdescriptor::EndpointType::kInterrupt);
  ss_int_ep.w_max_packet_size = htole16(1024);
  ss_int_ep.b_interval = 1;

  usb_ss_ep_comp_descriptor_t ss_comp = {};
  ss_comp.b_length = sizeof(usb_ss_ep_comp_descriptor_t);
  ss_comp.b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion);
  ss_comp.b_max_burst = 3;
  ss_comp.w_bytes_per_interval = htole16(2048);

  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &ss_int_ep, &ss_comp, true));

  const uint8_t ss_ep_index = XhciEndpointIndex(ss_int_ep.b_endpoint_address);
  {
    fbl::AutoLock l(&state->transaction_lock());
    auto* control = static_cast<uint8_t*>(state->GetInputContext()->virt());
    auto* ep_ctx = reinterpret_cast<EndpointContext*>(control + (slot_size * (2 + ss_ep_index)));
    EXPECT_EQ(ep_ctx->MAX_PACKET_SIZE(), 1024u);
    EXPECT_EQ(ep_ctx->MaxBurstSize(), 3u);
    EXPECT_EQ(ep_ctx->MAX_ESIT_PAYLOAD_LOW(), 2048u);
  }

  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &ss_int_ep, &ss_comp, false));

  // When w_bytes_per_interval is 0, MAX_ESIT_PAYLOAD_LOW falls back to
  // max_packet_size * (max_burst + 1) == 1024 * (3 + 1) == 4096.
  ss_comp.w_bytes_per_interval = 0;
  ASSERT_OK(driver_test().driver()->UsbHciEnableEndpoint(0, &ss_int_ep, &ss_comp, true));
  {
    fbl::AutoLock l(&state->transaction_lock());
    auto* control = static_cast<uint8_t*>(state->GetInputContext()->virt());
    auto* ep_ctx = reinterpret_cast<EndpointContext*>(control + (slot_size * (2 + ss_ep_index)));
    EXPECT_EQ(ep_ctx->MAX_PACKET_SIZE(), 1024u);
    EXPECT_EQ(ep_ctx->MaxBurstSize(), 3u);
    EXPECT_EQ(ep_ctx->MAX_ESIT_PAYLOAD_LOW(), 4096u);
  }
}

TEST_F(XhciMmioHarness, InitializesInputControlContextForAddressDeviceAndSetMaxPacketSize) {
  UsbXhci* xhci = driver_test().driver();
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(0, 1)), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(33, 1)), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(255, 1)), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(1, 1)), ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(32, 1)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(0, 1, std::nullopt, false)),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(33, 1, std::nullopt, false)),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(255, 1, std::nullopt, false)),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(1, 1, std::nullopt, false)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(32, 1, std::nullopt, false)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(0, 64)),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(33, 64)),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(255, 64)),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(1, 64)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(32, 64)),
                ZX_ERR_IO_NOT_PRESENT);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  uint32_t cmd_count = 0;
  uint32_t last_cmd_type = 0;
  zx_paddr_t last_cmd_ptr = 0;
  uint32_t last_cmd_slot = 0;
  uint32_t last_cmd_bsr = 1;
  uint32_t last_cmd_control = 0;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending) {
        ++cmd_count;
        if (cmd.trb) {
          last_cmd_type = Control::FromTRB(cmd.trb).Type();
          last_cmd_ptr = cmd.trb->ptr;
          last_cmd_control = cmd.trb->control;
          AddressDeviceStruct addr_cmd;
          static_cast<TRB&>(addr_cmd) = *cmd.trb;
          last_cmd_slot = addr_cmd.SlotID();
          if (last_cmd_type == Control::AddressDeviceCommand) {
            last_cmd_bsr = addr_cmd.BSR();
          }
        }
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  FakeUsbDevice dev = ConnectDevice(1, USB_SPEED_FULL);
  const uint8_t slot = static_cast<uint8_t>(dev.device_id + 1);
  fbl::RefPtr<DeviceState> state = xhci->GetDeviceState(dev.device_id);
  ASSERT_TRUE(state);

  // Verify the !state->GetInputContext() guard on a registered slot that has not run 4-arg
  // AddressDeviceCommand yet.
  const uint8_t unaddressed_slot = static_cast<uint8_t>((slot % 32) + 1);
  xhci->SetDeviceInformation(unaddressed_slot, 2, std::nullopt);
  cmd_count = 0;
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(unaddressed_slot, 2)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(unaddressed_slot, 64)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(cmd_count, 0u);

  zx_paddr_t expected_input_phys = 0;
  {
    fbl::AutoLock _(&state->transaction_lock());
    expected_input_phys = state->GetInputContext()->phys()[0];
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    control[0] = 0xdeadbeefu;
    control[1] = 0xffffffffu;
    auto* ep0_ctx = reinterpret_cast<EndpointContext*>(reinterpret_cast<unsigned char*>(control) +
                                                       (xhci->slot_size_bytes() * 2));
    ep0_ctx->Deinit();
    ep0_ctx->set_MAX_PACKET_SIZE(16);
  }

  cmd_count = 0;
  last_cmd_type = 0;
  last_cmd_ptr = 0;
  last_cmd_slot = 0;
  last_cmd_bsr = 1;
  last_cmd_control = 0;
  ASSERT_OK(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(slot, 1)));
  EXPECT_EQ(cmd_count, 1u);
  EXPECT_EQ(last_cmd_type, static_cast<uint32_t>(Control::AddressDeviceCommand));
  EXPECT_EQ(last_cmd_ptr, expected_input_phys);
  EXPECT_EQ(last_cmd_slot, slot);
  EXPECT_EQ(last_cmd_bsr, 0u);
  {
    fbl::AutoLock _(&state->transaction_lock());
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    EXPECT_EQ(control[0], 0u);
    EXPECT_EQ(control[1], 0x3u);
    auto* ep0_ctx = reinterpret_cast<EndpointContext*>(reinterpret_cast<unsigned char*>(control) +
                                                       (xhci->slot_size_bytes() * 2));
    EXPECT_EQ(ep0_ctx->EP_TYPE(), EndpointContext::Control);
    EXPECT_EQ(ep0_ctx->MAX_PACKET_SIZE(), 16u);
    EXPECT_EQ(ep0_ctx->CErr(), 3u);
    EXPECT_EQ(ep0_ctx->AVG_TRB_LENGTH(), 8u);
    EXPECT_EQ(ep0_ctx->DCS(), 1u);
    EXPECT_EQ(ep0_ctx->dequeue_pointer_a, static_cast<uint32_t>(kPeekPtr) | 1u);
    EXPECT_EQ(ep0_ctx->dequeue_pointer_b,
              static_cast<uint32_t>(static_cast<uint64_t>(kPeekPtr) >> 32u));
    control[0] = 0xfeedfaceu;
    control[1] = 0xffffffffu;
  }

  cmd_count = 0;
  last_cmd_type = 0;
  last_cmd_ptr = 0;
  last_cmd_slot = 0;
  last_cmd_control = 0;
  ASSERT_OK(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(slot, 64)));
  EXPECT_EQ(cmd_count, 1u);
  EXPECT_EQ(last_cmd_type, static_cast<uint32_t>(Control::EvaluateContextCommand));
  EXPECT_EQ(last_cmd_ptr, expected_input_phys);
  EXPECT_EQ(last_cmd_slot, slot);
  EXPECT_EQ((last_cmd_control >> 9u) & 1u, 0u);
  {
    fbl::AutoLock _(&state->transaction_lock());
    auto* control = static_cast<uint32_t*>(state->GetInputContext()->virt());
    EXPECT_EQ(control[0], 0u);
    EXPECT_EQ(control[1], 0x2u);
    auto* ep0_ctx = reinterpret_cast<EndpointContext*>(reinterpret_cast<unsigned char*>(control) +
                                                       (xhci->slot_size_bytes() * 2));
    EXPECT_EQ(ep0_ctx->MAX_PACKET_SIZE(), 64u);
    // Verify the state->IsDisconnecting() guard when InputContext is still present.
    state->Disconnect();
  }

  cmd_count = 0;
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(slot, 1)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->SetMaxPacketSizeCommand(slot, 64)),
                ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(cmd_count, 0u);
}

TEST_F(XhciMmioHarness, StopDrainsPendingControlAndTransferRequestsWithIoNotPresent) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  FakeUsbDevice dev = ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(dev.device_id, 1, true);
  xhci->RunUntilIdle();

  auto state = xhci->GetDeviceState(dev.device_id);
  ASSERT_TRUE(state);
  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 2;
  ASSERT_OK(xhci->UsbHciConfigureHub(dev.device_id, USB_SPEED_HIGH, &hub_desc, false));

  // Follow-up command TRB queued onto command_ring_ from an EP0 completion continuation must ALSO
  // be drained before StopDriver() returns (exercising a second DrainPendingOnce() pass in the
  // fixed-point DrainUntilQuiescent loop across stopped interrupters).
  struct CallbackState {
    UsbXhci* xhci = nullptr;
    zx_status_t ep0_status = ZX_OK;
    zx_status_t bulk_status = ZX_OK;
    zx_status_t chained_cmd_status = ZX_OK;
    bool chained_cmd_completed = false;
    libsync::Completion ep0_done;
    libsync::Completion bulk_done;
  };
  auto cb_state = std::make_shared<CallbackState>();
  cb_state->xhci = xhci;

  std::optional<TestRequest> ep0_req;
  std::optional<TestRequest> bulk_req;
  ASSERT_OK(AllocateRequest(
      &ep0_req, dev.device_id, zx_system_get_page_size(), 0, [cb_state](TestRequest req) {
        cb_state->ep0_status = req.request()->response.status;
        cb_state->xhci->ScheduleTask(
            0, fpromise::make_promise([cb_state]() -> fpromise::promise<void, zx_status_t> {
                 auto ctx = cb_state->xhci->GetCommandRing()->AllocateContext();
                 if (!ctx) {
                   return fpromise::make_error_promise<zx_status_t>(ZX_ERR_NO_MEMORY);
                 }
                 fpromise::bridge<TRB*, zx_status_t> bridge;
                 ctx->completer = std::move(bridge.completer);
                 TRB followup_trb{};
                 zx_status_t add_status =
                     cb_state->xhci->GetCommandRing()->AddTRB(followup_trb, std::move(ctx));
                 if (add_status != ZX_OK) {
                   return fpromise::make_error_promise<zx_status_t>(add_status);
                 }
                 return bridge.consumer.promise()
                     .then([cb_state](fpromise::result<TRB*, zx_status_t>& res)
                               -> fpromise::result<void, zx_status_t> {
                       cb_state->chained_cmd_completed = true;
                       cb_state->chained_cmd_status = res.is_ok() ? ZX_OK : res.error();
                       return fpromise::ok();
                     })
                     .box();
               }).box());
        cb_state->ep0_done.Signal();
      }));
  ASSERT_TRUE(ep0_req.has_value());
  ep0_req->request()->setup.bm_request_type = 0x80;
  ep0_req->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  ep0_req->request()->setup.w_length = 8;

  constexpr uint8_t kBulkInEp = 1u | 0x80u;
  ASSERT_OK(AllocateRequest(&bulk_req, dev.device_id, zx_system_get_page_size(), kBulkInEp,
                            [cb_state](TestRequest req) {
                              cb_state->bulk_status = req.request()->response.status;
                              cb_state->bulk_done.Signal();
                            }));
  ASSERT_TRUE(bulk_req.has_value());

  RequestQueue(std::move(*ep0_req));
  RequestQueue(std::move(*bulk_req));

  const uint8_t bulk_ep_index = static_cast<uint8_t>(XhciEndpointIndex(kBulkInEp) - 1);
  {
    fbl::AutoLock _(&state->transaction_lock());
    ASSERT_TRUE(state->GetEndpoint().transfer_ring().active());
    ASSERT_TRUE(state->GetEndpoint(bulk_ep_index).transfer_ring().active());
  }

  EXPECT_FALSE(cb_state->ep0_done.signaled());
  EXPECT_FALSE(cb_state->bulk_done.signaled());

  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());

  EXPECT_TRUE(cb_state->ep0_done.signaled());
  EXPECT_TRUE(cb_state->bulk_done.signaled());
  EXPECT_STATUS(cb_state->ep0_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(cb_state->bulk_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(cb_state->chained_cmd_completed);
  EXPECT_STATUS(cb_state->chained_cmd_status, ZX_ERR_IO_NOT_PRESENT);

  // Verify Stop() -> ResetHci() post-conditions once the controller halted cleanly.
  {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_TRUE(state->IsDisconnecting());
    EXPECT_FALSE(state->GetHubLocked().has_value());
    EXPECT_FALSE(state->GetEndpoint().transfer_ring().active());
    EXPECT_FALSE(state->GetEndpoint(bulk_ep_index).transfer_ring().active());
  }

  // Subsequent Banjo request queue calls after Stop() must complete immediately with
  // ZX_ERR_IO_NOT_PRESENT without enqueuing onto the stopped controller.
  auto late_status = std::make_shared<zx_status_t>(ZX_OK);
  auto late_done = std::make_shared<libsync::Completion>();
  std::optional<TestRequest> late_req;
  ASSERT_OK(AllocateRequest(&late_req, dev.device_id, zx_system_get_page_size(), kBulkInEp,
                            [late_status, late_done](TestRequest req) {
                              *late_status = req.request()->response.status;
                              late_done->Signal();
                            }));
  ASSERT_TRUE(late_req.has_value());
  RequestQueue(std::move(*late_req));
  EXPECT_TRUE(late_done->signaled());
  EXPECT_STATUS(*late_status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, StopDrainsCommandQueuedByScheduledInterrupterTaskWhenRingsInitiallyEmpty) {
  UsbXhci* xhci = driver_test().driver();
  auto completed = std::make_shared<bool>(false);
  auto status = std::make_shared<zx_status_t>(ZX_OK);

  // Schedule an un-pumped interrupter task when both command and transfer rings are initially
  // empty. DrainUntilQuiescent() must pump interrupters before checking DrainPendingOnce() so the
  // TRB queued by this task is drained before StopDriver() returns.
  xhci->ScheduleTask(
      0, fpromise::make_promise([xhci, completed, status] -> fpromise::promise<void, zx_status_t> {
           auto ctx = xhci->GetCommandRing()->AllocateContext();
           if (!ctx) {
             return fpromise::make_error_promise<zx_status_t>(ZX_ERR_NO_MEMORY);
           }
           fpromise::bridge<TRB*, zx_status_t> bridge;
           ctx->completer = std::move(bridge.completer);
           TRB cmd{};
           zx_status_t add_status = xhci->GetCommandRing()->AddTRB(cmd, std::move(ctx));
           if (add_status != ZX_OK) {
             return fpromise::make_error_promise<zx_status_t>(add_status);
           }
           return bridge.consumer.promise()
               .then([completed, status](fpromise::result<TRB*, zx_status_t>& res)
                         -> fpromise::result<void, zx_status_t> {
                 *completed = true;
                 *status = res.is_ok() ? ZX_OK : res.error();
                 return fpromise::ok();
               })
               .box();
         }).box());

  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());

  EXPECT_TRUE(*completed);
  EXPECT_STATUS(*status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, StopHaltTimeoutLeavesPendingTransfersAndRingsPinned) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  FakeUsbDevice dev = ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(dev.device_id, 1, true);
  xhci->RunUntilIdle();

  auto state = xhci->GetDeviceState(dev.device_id);
  ASSERT_TRUE(state);

  struct TransferState {
    zx_status_t ep0_status = ZX_OK;
    zx_status_t bulk_status = ZX_OK;
    libsync::Completion ep0_done;
    libsync::Completion bulk_done;
  };
  auto transfer_state = std::make_shared<TransferState>();

  std::optional<TestRequest> ep0_req;
  std::optional<TestRequest> bulk_req;
  ASSERT_OK(AllocateRequest(&ep0_req, dev.device_id, zx_system_get_page_size(), 0,
                            [transfer_state](TestRequest req) {
                              transfer_state->ep0_status = req.request()->response.status;
                              transfer_state->ep0_done.Signal();
                            }));
  ASSERT_TRUE(ep0_req.has_value());
  ep0_req->request()->setup.bm_request_type = 0x80;
  ep0_req->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  ep0_req->request()->setup.w_length = 8;

  constexpr uint8_t kBulkInEp = 1u | 0x80u;
  ASSERT_OK(AllocateRequest(&bulk_req, dev.device_id, zx_system_get_page_size(), kBulkInEp,
                            [transfer_state](TestRequest req) {
                              transfer_state->bulk_status = req.request()->response.status;
                              transfer_state->bulk_done.Signal();
                            }));
  ASSERT_TRUE(bulk_req.has_value());

  RequestQueue(std::move(*ep0_req));
  RequestQueue(std::move(*bulk_req));

  // Queue a pending software command TRB directly onto command_ring_ as well.
  auto cmd_ctx = xhci->GetCommandRing()->AllocateContext();
  ASSERT_TRUE(cmd_ctx);
  fpromise::bridge<TRB*, zx_status_t> cmd_bridge;
  cmd_ctx->completer = std::move(cmd_bridge.completer);
  auto cmd_done = std::make_shared<bool>(false);
  auto cmd_status = std::make_shared<zx_status_t>(ZX_OK);
  // Returning ZX_ERR_BAD_STATE from a continuation resumed during DrainUntilQuiescent() (mirroring
  // EnumerateDevice() seeing a disconnected slot) must not invoke Shutdown() and deadlock while
  // HCHalted == 0.
  xhci->ScheduleTask(0, cmd_bridge.consumer.promise()
                            .then([cmd_done, cmd_status](fpromise::result<TRB*, zx_status_t>& res)
                                      -> fpromise::result<void, zx_status_t> {
                              *cmd_done = true;
                              *cmd_status = res.is_ok() ? ZX_OK : res.error();
                              return fpromise::error(ZX_ERR_BAD_STATE);
                            })
                            .box());
  TRB cmd_trb{};
  ASSERT_OK(xhci->GetCommandRing()->AddTRB(cmd_trb, std::move(cmd_ctx)));

  // Simulate an unresponsive controller where USBSTS.HCHalted remains 0 during Stop().
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  auto allow_halt = std::make_shared<std::atomic<bool>>(false);
  auto restore_halt =
      fit::defer([allow_halt] { allow_halt->store(true, std::memory_order_relaxed); });
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbSts].SetReadCallback([allow_halt] {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(allow_halt->load(std::memory_order_relaxed) ? 1 : 0);
      return sts.reg_value();
    });
  });

  stopped_ = true;
  // DriverServer2::Stop ignores StopCompleter's error status when tearing down the FIDL binding
  // (StopWithHCHaltedTimeout verifies the ZX_ERR_TIMED_OUT status directly on StopCompleter).
  ASSERT_TRUE(driver_test().StopDriver().is_ok());
  // Direct Shutdown() after Stop() while HCHalted == 0 must return immediately without spinning.
  xhci->Shutdown(ZX_ERR_BAD_STATE);

  // Command TRBs drain (no client DMA unpin), slots are marked disconnecting, and active transfer
  // requests + transfer rings stay pinned while HCHalted == 0.
  const uint8_t bulk_ep_index = static_cast<uint8_t>(XhciEndpointIndex(kBulkInEp) - 1);
  EXPECT_TRUE(*cmd_done);
  EXPECT_STATUS(*cmd_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_FALSE(transfer_state->ep0_done.signaled());
  EXPECT_FALSE(transfer_state->bulk_done.signaled());
  {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_TRUE(state->IsDisconnecting());
    EXPECT_TRUE(state->GetEndpoint().transfer_ring().active());
    EXPECT_TRUE(state->GetEndpoint(bulk_ep_index).transfer_ring().active());
  }

  // Once hardware confirms HCHalted == 1 before destruction, ~UsbXhci() drains the pending
  // transfers and tears down the rings cleanly.
  allow_halt->store(true, std::memory_order_relaxed);
  driver_test().ShutdownAndDestroyDriver();

  EXPECT_TRUE(transfer_state->ep0_done.signaled());
  EXPECT_TRUE(transfer_state->bulk_done.signaled());
  EXPECT_STATUS(transfer_state->ep0_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(transfer_state->bulk_status, ZX_ERR_IO_NOT_PRESENT);
  {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_FALSE(state->GetEndpoint().transfer_ring().active());
    EXPECT_FALSE(state->GetEndpoint(bulk_ep_index).transfer_ring().active());
  }
}

TEST_F(XhciMmioHarness, StopDoesNotUseDestroyedDdkInteractionDispatcherDuringDrain) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&success_event);
        }
      }
    }
  });

  ConnectDevice(1, USB_SPEED_HIGH);
  xhci->RunUntilIdle();

  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());

  // After StopDriver() shuts down and destroys ddk_interaction_dispatcher_, PostCallback() must
  // reject new callbacks without touching the destroyed dispatcher.
  xhci->SetDeviceInformation(2, 2, std::nullopt);
  EXPECT_STATUS(xhci->DeviceOnline(2, 2, USB_SPEED_HIGH), ZX_ERR_IO_NOT_PRESENT);

  bool offline_completed = false;
  zx_status_t offline_status = ZX_ERR_INTERNAL;
  xhci->ScheduleTask(0, xhci->DeviceOffline(2)
                            .then([&](fpromise::result<void, zx_status_t>& res)
                                      -> fpromise::result<void, zx_status_t> {
                              offline_completed = true;
                              offline_status = res.is_ok() ? ZX_OK : res.error();
                              return fpromise::ok();
                            })
                            .box());
  xhci->RunUntilIdle(0);
  EXPECT_TRUE(offline_completed);
  EXPECT_STATUS(offline_status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, HubDeviceAddedAndRemovedHandleDisconnectingHubAndHaltedController) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  bool hold_commands = false;
  auto sts_halted = std::make_shared<std::atomic<bool>>(false);
  bool halt_on_doorbell = false;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      if (halt_on_doorbell) {
        sts_halted->store(true, std::memory_order_relaxed);
      }
      if (!hold_commands) {
        auto pending = xhci->GetCommandRing()->TakePendingTRBs();
        for (auto& cmd : pending) {
          if (cmd.completer.has_value()) {
            cmd.completer->complete_ok(&success_event);
          }
        }
      }
    }
  });

  FakeUsbDevice dev = ConnectDevice(1, USB_SPEED_HIGH);
  FakeUsbDevice child = ConnectDevice(2, USB_SPEED_FULL);
  xhci->RunUntilIdle();
  ASSERT_TRUE(xhci->Running());

  auto state = xhci->GetDeviceState(dev.device_id);
  ASSERT_TRUE(state);
  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 3;
  ASSERT_OK(xhci->UsbHciConfigureHub(dev.device_id, USB_SPEED_HIGH, &hub_desc, false));

  // While running_ == true, self-referencing port_to_device == hub_id is rejected before
  // CollectDisconnectSubtree() can disable the parent hub's own slot.
  {
    fbl::AutoLock _(&state->transaction_lock());
    auto& hub = state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    hub->port_to_device[0] = static_cast<uint8_t>(dev.device_id);
    hub->port_to_device[1] = static_cast<uint8_t>(child.device_id);
  }
  EXPECT_STATUS(xhci->UsbHciHubDeviceRemoved(dev.device_id, 1), ZX_ERR_IO_NOT_PRESENT);

  // While running_ == true, if hardware sets USBSTS.HCH = 1 after DisableSlotCommand rings
  // doorbell 0, UsbHciHubDeviceRemoved's RunSynchronously() aborts cleanly without hanging or
  // touching a popped stack-allocated targets vector.
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbSts].SetReadCallback([sts_halted] {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(sts_halted->load(std::memory_order_relaxed) ? 1 : 0);
      return sts.reg_value();
    });
  });
  hold_commands = true;
  halt_on_doorbell = true;
  ASSERT_TRUE(xhci->Running());
  EXPECT_OK(xhci->UsbHciHubDeviceRemoved(dev.device_id, 2));
  EXPECT_TRUE(sts_halted->load(std::memory_order_relaxed));

  // Reset HCHalted to 0 so EnableSlotCommand() passes SubmitCommand() and rings doorbell 0 (which
  // sets HCHalted = 1), exercising the RunSynchronously() abort loop in UsbHciHubDeviceAdded().
  sts_halted->store(false, std::memory_order_relaxed);
  EXPECT_STATUS(xhci->UsbHciHubDeviceAdded(dev.device_id, 3, USB_SPEED_LOW), ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(sts_halted->load(std::memory_order_relaxed));

  // While running_ == true and hub_ is still populated, marking hub_state disconnecting causes
  // UsbHciHubDeviceRemoved to return ZX_OK immediately via hub_state->IsDisconnecting().
  {
    fbl::AutoLock _(&state->transaction_lock());
    ASSERT_TRUE(state->GetHubLocked().has_value());
    state->Disconnect();
  }
  ASSERT_TRUE(xhci->Running());
  EXPECT_OK(xhci->UsbHciHubDeviceRemoved(dev.device_id, 3));
}

TEST_F(XhciMmioHarness, RunSynchronouslyAbortsTransfersOnlyWhenControllerHalted) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  auto cmd_enabled = std::make_shared<std::atomic<bool>>(true);
  auto sts_halted = std::make_shared<std::atomic<bool>>(false);
  bool hold_commands = false;
  bool clear_enable_on_doorbell = false;
  bool set_halted_on_doorbell = false;

  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      if (clear_enable_on_doorbell) {
        cmd_enabled->store(false, std::memory_order_relaxed);
      }
      if (set_halted_on_doorbell) {
        sts_halted->store(true, std::memory_order_relaxed);
      }
      if (!hold_commands) {
        auto pending = xhci->GetCommandRing()->TakePendingTRBs();
        for (auto& cmd : pending) {
          if (cmd.completer.has_value()) {
            cmd.completer->complete_ok(&success_event);
          }
        }
      }
    }
  });

  FakeUsbDevice dev = ConnectDevice(1, USB_SPEED_HIGH);
  EnableEndpoint(dev.device_id, 1, true);
  xhci->RunUntilIdle();
  ASSERT_TRUE(xhci->Running());

  struct TransferState {
    zx_status_t ep0_status = ZX_OK;
    zx_status_t bulk_status = ZX_OK;
    libsync::Completion ep0_done;
    libsync::Completion bulk_done;
  };
  auto transfer_state = std::make_shared<TransferState>();

  std::optional<TestRequest> ep0_req;
  std::optional<TestRequest> bulk_req;
  ASSERT_OK(AllocateRequest(&ep0_req, dev.device_id, zx_system_get_page_size(), 0,
                            [transfer_state](TestRequest req) {
                              transfer_state->ep0_status = req.request()->response.status;
                              transfer_state->ep0_done.Signal();
                            }));
  ASSERT_TRUE(ep0_req.has_value());
  ep0_req->request()->setup.bm_request_type = 0x80;
  ep0_req->request()->setup.b_request =
      fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  ep0_req->request()->setup.w_length = 8;

  constexpr uint8_t kBulkInEp = 1u | 0x80u;
  ASSERT_OK(AllocateRequest(&bulk_req, dev.device_id, zx_system_get_page_size(), kBulkInEp,
                            [transfer_state](TestRequest req) {
                              transfer_state->bulk_status = req.request()->response.status;
                              transfer_state->bulk_done.Signal();
                            }));
  ASSERT_TRUE(bulk_req.has_value());

  RequestQueue(std::move(*ep0_req));
  RequestQueue(std::move(*bulk_req));

  constexpr auto kUsbCmd = 7 * sizeof(uint32_t);
  constexpr auto kUsbSts = 8 * sizeof(uint32_t);
  constexpr uint8_t kCapLength = 0x1c;
  driver_test().RunInEnvironmentTypeContext([&](XhciTestEnvironment& env) {
    env.fake_device().region()[kUsbCmd].SetReadCallback([cmd_enabled] {
      auto cmd = USBCMD::Get(kCapLength).FromValue(0);
      cmd.set_ENABLE(cmd_enabled->load(std::memory_order_relaxed) ? 1 : 0);
      return cmd.reg_value();
    });
    env.fake_device().region()[kUsbCmd].SetWriteCallback([cmd_enabled](uint64_t value) {
      auto cmd = USBCMD::Get(kCapLength).FromValue(static_cast<uint32_t>(value));
      cmd_enabled->store(cmd.ENABLE() != 0, std::memory_order_relaxed);
    });
    env.fake_device().region()[kUsbSts].SetReadCallback([sts_halted] {
      auto sts = USBSTS::Get(kCapLength).FromValue(0);
      sts.set_HCHalted(sts_halted->load(std::memory_order_relaxed) ? 1 : 0);
      return sts.reg_value();
    });
  });

  // Case 1: USBCMD.ENABLE transitions to 0 after SubmitCommand() queues a command TRB while
  // USBSTS.HCH remains 0. AbortPendingCommands() must drain the command TRB with
  // ZX_ERR_IO_NOT_PRESENT, but must NOT complete/unpin EP0 or bulk transfer requests while
  // HCHalted == 0.
  hold_commands = true;
  clear_enable_on_doorbell = true;
  set_halted_on_doorbell = false;
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->EnableSlotCommand()), ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(xhci->Running());
  EXPECT_FALSE(cmd_enabled->load(std::memory_order_relaxed));
  EXPECT_FALSE(sts_halted->load(std::memory_order_relaxed));
  EXPECT_FALSE(transfer_state->ep0_done.signaled());
  EXPECT_FALSE(transfer_state->bulk_done.signaled());

  // Case 2: While running_ == true, USBSTS.HCH transitions to 1 after SubmitCommand() queues a
  // command TRB. AbortPendingCommands() -> DrainPendingOnce() must drain both the command TRB and
  // the pending EP0 and bulk transfer requests with ZX_ERR_IO_NOT_PRESENT.
  cmd_enabled->store(true, std::memory_order_relaxed);
  clear_enable_on_doorbell = false;
  set_halted_on_doorbell = true;
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->EnableSlotCommand()), ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(xhci->Running());
  EXPECT_TRUE(sts_halted->load(std::memory_order_relaxed));
  EXPECT_TRUE(transfer_state->ep0_done.signaled());
  EXPECT_TRUE(transfer_state->bulk_done.signaled());
  EXPECT_STATUS(transfer_state->ep0_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(transfer_state->bulk_status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, DisableSlotFailureWhileRunningSkipsRemoveDeviceAndKeepsDmaPinned) {
  UsbXhci* xhci = driver_test().driver();
  struct DoorbellState {
    CommandCompletionEvent success_event;
    CommandCompletionEvent aborted_event;
    std::atomic<bool> fail_commands{false};
  };
  auto doorbell_state = std::make_shared<DoorbellState>();
  doorbell_state->success_event.set_CompletionCode(CommandCompletionEvent::Success);
  doorbell_state->aborted_event.set_CompletionCode(CommandCompletionEvent::CommandAborted);

  SetDoorbellListener([xhci, doorbell_state](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      CommandCompletionEvent* event = doorbell_state->fail_commands.load(std::memory_order_relaxed)
                                          ? &doorbell_state->aborted_event
                                          : &doorbell_state->success_event;
      for (auto& cmd : pending) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(event);
        }
      }
    }
  });

  FakeUsbDevice root_dev = ConnectDevice(1, USB_SPEED_HIGH);
  FakeUsbDevice hub_dev = ConnectDevice(2, USB_SPEED_HIGH);
  FakeUsbDevice child_dev = ConnectDevice(3, USB_SPEED_FULL);
  EnableEndpoint(root_dev.device_id, 1, true);
  EnableEndpoint(child_dev.device_id, 1, true);
  xhci->RunUntilIdle();

  auto root_state = xhci->GetDeviceState(root_dev.device_id);
  auto hub_state = xhci->GetDeviceState(hub_dev.device_id);
  auto child_state = xhci->GetDeviceState(child_dev.device_id);
  ASSERT_TRUE(root_state);
  ASSERT_TRUE(hub_state);
  ASSERT_TRUE(child_state);

  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 2;
  ASSERT_OK(xhci->UsbHciConfigureHub(hub_dev.device_id, USB_SPEED_HIGH, &hub_desc, false));
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    auto& hub = hub_state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    hub->port_to_device[0] = static_cast<uint8_t>(child_dev.device_id);
  }

  struct TransferState {
    zx_status_t root_ep0_status = ZX_OK;
    zx_status_t root_bulk_status = ZX_OK;
    zx_status_t child_ep0_status = ZX_OK;
    zx_status_t child_bulk_status = ZX_OK;
    libsync::Completion root_ep0_done;
    libsync::Completion root_bulk_done;
    libsync::Completion child_ep0_done;
    libsync::Completion child_bulk_done;
  };
  auto transfer_state = std::make_shared<TransferState>();

  constexpr uint8_t kBulkInEp = 1u | 0x80u;
  std::optional<TestRequest> root_ep0_req;
  std::optional<TestRequest> root_bulk_req;
  std::optional<TestRequest> child_ep0_req;
  std::optional<TestRequest> child_bulk_req;
  ASSERT_OK(AllocateRequest(&root_ep0_req, root_dev.device_id, zx_system_get_page_size(), 0,
                            [transfer_state](TestRequest req) {
                              transfer_state->root_ep0_status = req.request()->response.status;
                              transfer_state->root_ep0_done.Signal();
                            }));
  ASSERT_OK(AllocateRequest(&root_bulk_req, root_dev.device_id, zx_system_get_page_size(),
                            kBulkInEp, [transfer_state](TestRequest req) {
                              transfer_state->root_bulk_status = req.request()->response.status;
                              transfer_state->root_bulk_done.Signal();
                            }));
  ASSERT_OK(AllocateRequest(&child_ep0_req, child_dev.device_id, zx_system_get_page_size(), 0,
                            [transfer_state](TestRequest req) {
                              transfer_state->child_ep0_status = req.request()->response.status;
                              transfer_state->child_ep0_done.Signal();
                            }));
  ASSERT_OK(AllocateRequest(&child_bulk_req, child_dev.device_id, zx_system_get_page_size(),
                            kBulkInEp, [transfer_state](TestRequest req) {
                              transfer_state->child_bulk_status = req.request()->response.status;
                              transfer_state->child_bulk_done.Signal();
                            }));
  ASSERT_TRUE(root_ep0_req.has_value());
  ASSERT_TRUE(root_bulk_req.has_value());
  ASSERT_TRUE(child_ep0_req.has_value());
  ASSERT_TRUE(child_bulk_req.has_value());

  for (TestRequest* req : {&*root_ep0_req, &*child_ep0_req}) {
    req->request()->setup.bm_request_type = 0x80;
    req->request()->setup.b_request =
        fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
    req->request()->setup.w_length = 8;
  }

  RequestQueue(std::move(*root_ep0_req));
  RequestQueue(std::move(*root_bulk_req));
  RequestQueue(std::move(*child_ep0_req));
  RequestQueue(std::move(*child_bulk_req));

  // Fail DisableSlotCommand with CommandAborted while the controller is still running.
  doorbell_state->fail_commands.store(true, std::memory_order_relaxed);

  // DeviceOffline() must not unpin/drain active transfers or call bus_.RemoveDevice() while
  // hardware is running and DisableSlotCommand failed.
  auto offline_done = std::make_shared<bool>(false);
  auto offline_status = std::make_shared<zx_status_t>(ZX_OK);
  xhci->ScheduleTask(
      0, xhci->DeviceOffline(root_dev.device_id + 1)
             .then([offline_done, offline_status](fpromise::result<void, zx_status_t>& res)
                       -> fpromise::result<void, zx_status_t> {
               *offline_done = true;
               *offline_status = res.is_ok() ? ZX_OK : res.error();
               return fpromise::ok();
             })
             .box());
  xhci->RunUntilIdle(0);
  EXPECT_TRUE(*offline_done);
  EXPECT_STATUS(*offline_status, ZX_ERR_IO);
  EXPECT_EQ(bus_.devices().count(root_dev.device_id), 1u);
  EXPECT_EQ(xhci->GetDeviceState(root_dev.device_id), root_state);
  EXPECT_FALSE(transfer_state->root_ep0_done.signaled());
  EXPECT_FALSE(transfer_state->root_bulk_done.signaled());

  // UsbHciHubDeviceRemoved() must likewise skip RemoveDevice() and leave DMA pinned.
  EXPECT_STATUS(xhci->UsbHciHubDeviceRemoved(hub_dev.device_id, 1), ZX_ERR_IO);
  EXPECT_EQ(bus_.devices().count(child_dev.device_id), 1u);
  EXPECT_EQ(xhci->GetDeviceState(child_dev.device_id), child_state);
  EXPECT_FALSE(transfer_state->child_ep0_done.signaled());
  EXPECT_FALSE(transfer_state->child_bulk_done.signaled());

  const uint8_t bulk_ep_index = static_cast<uint8_t>(XhciEndpointIndex(kBulkInEp) - 1);
  for (const auto& state : {root_state, child_state}) {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_TRUE(state->IsDisconnecting());
    EXPECT_TRUE(state->GetEndpoint().transfer_ring().active());
    EXPECT_TRUE(state->GetEndpoint(bulk_ep_index).transfer_ring().active());
  }

  // Once the controller halts during StopDriver(), all remaining pinned transfers drain cleanly.
  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());
  driver_test().ShutdownAndDestroyDriver();

  EXPECT_TRUE(transfer_state->root_ep0_done.signaled());
  EXPECT_TRUE(transfer_state->root_bulk_done.signaled());
  EXPECT_TRUE(transfer_state->child_ep0_done.signaled());
  EXPECT_TRUE(transfer_state->child_bulk_done.signaled());
  EXPECT_STATUS(transfer_state->root_ep0_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(transfer_state->root_bulk_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(transfer_state->child_ep0_status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_STATUS(transfer_state->child_bulk_status, ZX_ERR_IO_NOT_PRESENT);
  for (const auto& state : {root_state, child_state}) {
    fbl::AutoLock _(&state->transaction_lock());
    EXPECT_FALSE(state->GetEndpoint().transfer_ring().active());
    EXPECT_FALSE(state->GetEndpoint(bulk_ep_index).transfer_ring().active());
  }
}

TEST_F(XhciMmioHarness, ConcurrentDisableSlotAndDeviceOfflineAwaitInFlightHardwareCommand) {
  UsbXhci* xhci = driver_test().driver();
  const zx_paddr_t dcbaa_phys = driver_test().RunInEnvironmentTypeContext<zx_paddr_t>(
      [](XhciTestEnvironment& env) { return env.fake_device().dcbaa_phys(); });
  ASSERT_NE(dcbaa_phys, 0u);
  auto* dcbaa = ddk_fake::PhysToVirt<uint64_t*>(dcbaa_phys);

  CommandCompletionEvent success_event;
  success_event.set_CompletionCode(CommandCompletionEvent::Success);
  CommandCompletionEvent error_event;
  error_event.set_CompletionCode(CommandCompletionEvent::ContextStateError);

  uint8_t held_slot = 0;
  uint32_t held_slot_disable_count = 0;
  std::optional<fpromise::completer<TRB*, zx_status_t>> held_disable_completer;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell != 0) {
      return;
    }
    auto pending = xhci->GetCommandRing()->TakePendingTRBs();
    for (auto& cmd : pending) {
      if (cmd.trb && Control::FromTRB(cmd.trb).Type() == Control::DisableSlot) {
        DisableSlot disable_cmd;
        static_cast<TRB&>(disable_cmd) = *cmd.trb;
        if (held_slot != 0 && disable_cmd.slot() == held_slot) {
          ++held_slot_disable_count;
          held_disable_completer = std::move(cmd.completer);
          continue;
        }
      }
      if (cmd.completer.has_value()) {
        cmd.completer->complete_ok(&success_event);
      }
    }
  });
  auto cleanup_listener = fit::defer([&] {
    SetDoorbellListener([](uint8_t, uint8_t) {});
    if (held_disable_completer.has_value()) {
      held_disable_completer->complete_error(ZX_ERR_IO_NOT_PRESENT);
      held_disable_completer.reset();
      xhci->RunUntilIdle();
    }
  });

  // Connect a root-attached hub and a downstream child with an active endpoint transfer.
  FakeUsbDevice hub_dev = ConnectDevice(1, USB_SPEED_HIGH);
  const uint32_t hub_id = hub_dev.device_id;
  const uint8_t hub_slot = static_cast<uint8_t>(hub_id + 1);
  fbl::RefPtr<DeviceState> hub_state = xhci->GetDeviceState(hub_id);
  ASSERT_TRUE(hub_state);
  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 4;
  ASSERT_OK(xhci->UsbHciConfigureHub(hub_id, USB_SPEED_HIGH, &hub_desc, true));

  const uint8_t child_slot = AllocateSlot();
  const uint32_t child_id = child_slot - 1u;
  HubInfo info = {};
  info.hub_state = hub_state;
  info.speed = USB_SPEED_FULL;
  info.hub_speed = USB_SPEED_HIGH;
  info.rh_port = 1;
  info.parent_port_number = 3;
  xhci->SetDeviceInformation(child_slot, 3, info);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(child_slot, 3, info, true));
  xhci->RunUntilIdle();
  fbl::RefPtr<DeviceState> child_state = xhci->GetDeviceState(child_id);
  ASSERT_TRUE(child_state);
  ASSERT_NE(dcbaa[child_slot], 0u);
  EnableEndpoint(child_id, 2, false);

  struct PromiseOutcome {
    bool done = false;
    zx_status_t status = ZX_ERR_INTERNAL;
  };
  auto record_outcome = [](std::shared_ptr<PromiseOutcome> outcome) {
    return [outcome = std::move(outcome)](fpromise::result<void, zx_status_t>& result) {
      outcome->done = true;
      outcome->status = result.is_ok() ? ZX_OK : result.error();
      return result;
    };
  };

  struct BanjoCbState {
    size_t parent_req_size = 0;
    zx_status_t status = ZX_OK;
    libsync::Completion completed;
  };
  auto banjo_state = std::make_shared<BanjoCbState>();
  banjo_state->parent_req_size = xhci->UsbHciGetRequestSize();
  std::optional<usb::Request<void>> banjo_req;
  ASSERT_OK(usb::Request<void>::Alloc(&banjo_req, zx_system_get_page_size(), 0x02,
                                      banjo_state->parent_req_size));
  ASSERT_TRUE(banjo_req.has_value());
  banjo_req->request()->header.device_id = child_id;
  banjo_req->request()->header.ep_address = 0x02;
  banjo_req->request()->header.length = zx_system_get_page_size();
  auto* cb_state_copy = std::make_unique<std::shared_ptr<BanjoCbState>>(banjo_state).release();
  usb_request_complete_callback_t banjo_cb = {
      .callback =
          [](void* ctx, usb_request_t* req) {
            std::unique_ptr<std::shared_ptr<BanjoCbState>> state_holder(
                static_cast<std::shared_ptr<BanjoCbState>*>(ctx));
            (*state_holder)->status = req->response.status;
            usb::Request<void> owned_req(req, (*state_holder)->parent_req_size);
            (*state_holder)->completed.Signal();
          },
      .ctx = cb_state_copy,
  };
  xhci->UsbHciRequestQueue(banjo_req->take(), &banjo_cb);

  // Hold the child's hardware Disable Slot command in flight.
  held_slot = child_slot;
  auto first_outcome = std::make_shared<PromiseOutcome>();
  xhci->ScheduleTask(0, xhci->DisableSlotCommand(*child_state, child_state)
                            .then(record_outcome(first_outcome))
                            .box());
  xhci->RunUntilIdle();
  ASSERT_EQ(held_slot_disable_count, 1u);
  ASSERT_TRUE(held_disable_completer.has_value());
  EXPECT_FALSE(first_outcome->done);
  EXPECT_NE(dcbaa[child_slot], 0u);

  // Issue both a direct concurrent DisableSlotCommand(*child_state) and DeviceOffline(hub_slot)
  // while the child's Disable Slot command is still in flight. Hub Disable Slot completes
  // immediately, but neither the direct waiter nor DeviceOffline may resolve or drain/reset the
  // child until the child's hardware Disable Slot command completes.
  auto second_outcome = std::make_shared<PromiseOutcome>();
  xhci->ScheduleTask(0, xhci->DisableSlotCommand(*child_state, child_state)
                            .then(record_outcome(second_outcome))
                            .box());

  bus_.reset_remove();
  struct OfflineState {
    zx_status_t status = ZX_ERR_INTERNAL;
    libsync::Completion completed;
  };
  auto offline_state = std::make_shared<OfflineState>();
  xhci->ScheduleTask(0, xhci->DeviceOffline(hub_slot)
                            .then([offline_state](fpromise::result<void, zx_status_t>& result) {
                              offline_state->status = result.is_ok() ? ZX_OK : result.error();
                              offline_state->completed.Signal();
                              return result;
                            })
                            .box());
  xhci->RunUntilIdle();

  EXPECT_EQ(held_slot_disable_count, 1u);
  EXPECT_FALSE(first_outcome->done);
  EXPECT_FALSE(second_outcome->done);
  EXPECT_FALSE(offline_state->completed.signaled());
  EXPECT_FALSE(child_state->slot_disabled_.signaled());
  EXPECT_NE(dcbaa[child_slot], 0u);
  EXPECT_EQ(xhci->GetDeviceState(child_id).get(), child_state.get());
  EXPECT_FALSE(banjo_state->completed.signaled());

  // Complete the in-flight Disable Slot command: all waiters resolve and DeviceOffline now safely
  // drains the pending transfer request and resets DeviceState.
  held_disable_completer->complete_ok(&success_event);
  held_disable_completer.reset();
  xhci->RunUntilIdle();
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([this] { bus_.wait_for_remove(); }));
  const zx::time deadline = zx::deadline_after(zx::sec(30));
  while (!offline_state->completed.signaled()) {
    ASSERT_LT(zx::clock::get_monotonic(), deadline)
        << "Timed out waiting for DeviceOffline(" << static_cast<uint32_t>(hub_slot) << ")";
    xhci->RunUntilIdle();
    if (!offline_state->completed.signaled()) {
      zx::nanosleep(zx::deadline_after(zx::usec(50)));
    }
  }

  EXPECT_TRUE(first_outcome->done);
  EXPECT_OK(first_outcome->status);
  EXPECT_TRUE(second_outcome->done);
  EXPECT_OK(second_outcome->status);
  EXPECT_OK(offline_state->status);
  EXPECT_EQ(dcbaa[child_slot], 0u);
  EXPECT_TRUE(child_state->slot_disabled_.signaled());
  EXPECT_FALSE(xhci->GetDeviceState(child_id));
  EXPECT_FALSE(xhci->GetDeviceState(hub_id));
  EXPECT_OK(banjo_state->completed.Wait(zx::sec(5)));
  EXPECT_STATUS(banjo_state->status, ZX_ERR_IO_NOT_PRESENT);

  // Subsequent DisableSlotCommand after completion resolves immediately without submitting another
  // hardware command.
  EXPECT_OK(xhci->RunSynchronously(0, xhci->DisableSlotCommand(*child_state, child_state)));
  EXPECT_EQ(held_slot_disable_count, 1u);

  // Hardware Disable Slot errors propagate to concurrent waiters and subsequent callers.
  const uint8_t err_slot = AllocateSlot();
  held_slot = err_slot;
  held_slot_disable_count = 0;
  xhci->SetDeviceInformation(err_slot, 2, std::nullopt);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(err_slot, 2, std::nullopt, true));
  xhci->RunUntilIdle();
  fbl::RefPtr<DeviceState> err_state = xhci->GetDeviceState(err_slot - 1u);
  ASSERT_TRUE(err_state);

  auto err_first = std::make_shared<PromiseOutcome>();
  auto err_second = std::make_shared<PromiseOutcome>();
  xhci->ScheduleTask(
      0, xhci->DisableSlotCommand(*err_state, err_state).then(record_outcome(err_first)).box());
  xhci->ScheduleTask(
      0, xhci->DisableSlotCommand(*err_state, err_state).then(record_outcome(err_second)).box());
  xhci->RunUntilIdle();
  ASSERT_EQ(held_slot_disable_count, 1u);
  ASSERT_TRUE(held_disable_completer.has_value());
  EXPECT_FALSE(err_first->done);
  EXPECT_FALSE(err_second->done);

  held_disable_completer->complete_ok(&error_event);
  held_disable_completer.reset();
  xhci->RunUntilIdle();
  EXPECT_TRUE(err_first->done);
  EXPECT_STATUS(err_first->status, ZX_ERR_IO);
  EXPECT_TRUE(err_second->done);
  EXPECT_STATUS(err_second->status, ZX_ERR_IO);
  EXPECT_TRUE(err_state->slot_disabled_.signaled());
  EXPECT_NE(dcbaa[err_slot], 0u);
  EXPECT_STATUS(xhci->RunSynchronously(0, xhci->DisableSlotCommand(*err_state, err_state)),
                ZX_ERR_IO);
  EXPECT_EQ(held_slot_disable_count, 1u);

  // SlotNotEnabledError clears dcbaa_[slot] and resolves both initiator and waiter with ZX_OK.
  CommandCompletionEvent slot_not_enabled_event;
  slot_not_enabled_event.set_CompletionCode(CommandCompletionEvent::SlotNotEnabledError);
  const uint8_t not_enabled_slot = AllocateSlot();
  held_slot = not_enabled_slot;
  held_slot_disable_count = 0;
  xhci->SetDeviceInformation(not_enabled_slot, 2, std::nullopt);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(not_enabled_slot, 2, std::nullopt, true));
  xhci->RunUntilIdle();
  fbl::RefPtr<DeviceState> not_enabled_state = xhci->GetDeviceState(not_enabled_slot - 1u);
  ASSERT_TRUE(not_enabled_state);
  ASSERT_NE(dcbaa[not_enabled_slot], 0u);

  auto not_enabled_first = std::make_shared<PromiseOutcome>();
  auto not_enabled_second = std::make_shared<PromiseOutcome>();
  xhci->ScheduleTask(0, xhci->DisableSlotCommand(*not_enabled_state, not_enabled_state)
                            .then(record_outcome(not_enabled_first))
                            .box());
  xhci->ScheduleTask(0, xhci->DisableSlotCommand(*not_enabled_state, not_enabled_state)
                            .then(record_outcome(not_enabled_second))
                            .box());
  xhci->RunUntilIdle();
  ASSERT_EQ(held_slot_disable_count, 1u);
  ASSERT_TRUE(held_disable_completer.has_value());
  EXPECT_FALSE(not_enabled_first->done);
  EXPECT_FALSE(not_enabled_second->done);

  held_disable_completer->complete_ok(&slot_not_enabled_event);
  held_disable_completer.reset();
  xhci->RunUntilIdle();
  EXPECT_TRUE(not_enabled_first->done);
  EXPECT_OK(not_enabled_first->status);
  EXPECT_TRUE(not_enabled_second->done);
  EXPECT_OK(not_enabled_second->status);
  EXPECT_TRUE(not_enabled_state->slot_disabled_.signaled());
  EXPECT_EQ(dcbaa[not_enabled_slot], 0u);

  // Dropping an initiator promise while a concurrent waiter is queued triggers the fit::defer
  // cleanup: the waiter resolves with ZX_ERR_IO_NOT_PRESENT, slot_disabled_ signals, and
  // dcbaa_[slot] is left untouched.
  const uint8_t drop_slot = AllocateSlot();
  held_slot = drop_slot;
  held_slot_disable_count = 0;
  xhci->SetDeviceInformation(drop_slot, 2, std::nullopt);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(drop_slot, 2, std::nullopt, true));
  xhci->RunUntilIdle();
  fbl::RefPtr<DeviceState> drop_state = xhci->GetDeviceState(drop_slot - 1u);
  ASSERT_TRUE(drop_state);
  const uint64_t drop_initial_dcbaa = dcbaa[drop_slot];
  ASSERT_NE(drop_initial_dcbaa, 0u);

  auto drop_waiter = std::make_shared<PromiseOutcome>();
  {
    auto dropped_initiator = xhci->DisableSlotCommand(*drop_state, drop_state);
    ASSERT_EQ(held_slot_disable_count, 1u);
    ASSERT_TRUE(held_disable_completer.has_value());
    xhci->ScheduleTask(
        0,
        xhci->DisableSlotCommand(*drop_state, drop_state).then(record_outcome(drop_waiter)).box());
    xhci->RunUntilIdle();
    EXPECT_FALSE(drop_waiter->done);
    EXPECT_FALSE(drop_state->slot_disabled_.signaled());
  }
  xhci->RunUntilIdle();
  EXPECT_TRUE(drop_waiter->done);
  EXPECT_STATUS(drop_waiter->status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(drop_state->slot_disabled_.signaled());
  EXPECT_EQ(dcbaa[drop_slot], drop_initial_dcbaa);
  held_disable_completer->complete_error(ZX_ERR_IO_NOT_PRESENT);
  held_disable_completer.reset();
  xhci->RunUntilIdle();

  // ResetHci() wakes any queued DisableSlotCommand waiter with ZX_ERR_IO_NOT_PRESENT.
  const uint8_t reset_slot = AllocateSlot();
  held_slot = reset_slot;
  held_slot_disable_count = 0;
  xhci->SetDeviceInformation(reset_slot, 2, std::nullopt);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(reset_slot, 2, std::nullopt, true));
  xhci->RunUntilIdle();
  fbl::RefPtr<DeviceState> reset_state = xhci->GetDeviceState(reset_slot - 1u);
  ASSERT_TRUE(reset_state);

  auto reset_waiter = std::make_shared<PromiseOutcome>();
  xhci->ScheduleTask(0, xhci->DisableSlotCommand(*reset_state, reset_state));
  xhci->ScheduleTask(
      0,
      xhci->DisableSlotCommand(*reset_state, reset_state).then(record_outcome(reset_waiter)).box());
  xhci->RunUntilIdle();
  ASSERT_TRUE(held_disable_completer.has_value());
  EXPECT_FALSE(reset_waiter->done);

  reset_state->ResetHci();
  xhci->RunUntilIdle();
  EXPECT_TRUE(reset_waiter->done);
  EXPECT_STATUS(reset_waiter->status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(reset_state->slot_disabled_.signaled());
  held_disable_completer->complete_error(ZX_ERR_IO_NOT_PRESENT);
  held_disable_completer.reset();
  xhci->RunUntilIdle();

  // Once Shutdown(ZX_OK) sets !running_ and halts the controller (before StopDriver() calls
  // ResetAllDevices()), DisableSlotCommand on an un-started slot takes the !running_ early-exit
  // path (clearing dcbaa_[slot], signaling slot_disabled_, and returning ZX_ERR_IO_NOT_PRESENT),
  // while an already in-flight DisableSlotCommand and its queued waiter stay pending until
  // StopDriver() aborts pending commands (leaving dcbaa_[slot] non-zero). Calling
  // DisableSlotCommand on an already completed DeviceState returns its cached status without
  // clobbering a reused dcbaa_[slot] entry.
  const uint8_t stop_slot = AllocateSlot();
  const uint8_t unstarted_slot = AllocateSlot();
  held_slot = 0;
  xhci->SetDeviceInformation(stop_slot, 2, std::nullopt);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(stop_slot, 2, std::nullopt, true));
  xhci->SetDeviceInformation(unstarted_slot, 2, std::nullopt);
  xhci->ScheduleTask(0, xhci->AddressDeviceCommand(unstarted_slot, 2, std::nullopt, true));
  xhci->RunUntilIdle();
  fbl::RefPtr<DeviceState> stop_state = xhci->GetDeviceState(stop_slot - 1u);
  fbl::RefPtr<DeviceState> unstarted_state = xhci->GetDeviceState(unstarted_slot - 1u);
  ASSERT_TRUE(stop_state);
  ASSERT_TRUE(unstarted_state);
  ASSERT_NE(dcbaa[stop_slot], 0u);
  ASSERT_NE(dcbaa[unstarted_slot], 0u);

  // Stop intercepting command ring TRBs so stop_slot's Disable Slot stays pending in command_ring_
  // for StopDriver() to abort after halting the controller.
  SetDoorbellListener([](uint8_t, uint8_t) {});
  auto stop_waiter = std::make_shared<PromiseOutcome>();
  xhci->ScheduleTask(0, xhci->DisableSlotCommand(*stop_state, stop_state));
  xhci->ScheduleTask(
      0, xhci->DisableSlotCommand(*stop_state, stop_state).then(record_outcome(stop_waiter)).box());
  xhci->RunUntilIdle();
  EXPECT_FALSE(stop_waiter->done);

  xhci->Shutdown(ZX_OK);
  EXPECT_FALSE(stop_waiter->done);
  EXPECT_STATUS(
      xhci->RunSynchronously(0, xhci->DisableSlotCommand(*unstarted_state, unstarted_state)),
      ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(unstarted_state->slot_disabled_.signaled());
  EXPECT_EQ(dcbaa[unstarted_slot], 0u);

  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());
  EXPECT_TRUE(stop_waiter->done);
  EXPECT_STATUS(stop_waiter->status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_TRUE(stop_state->slot_disabled_.signaled());
  EXPECT_NE(dcbaa[stop_slot], 0u);

  constexpr uint64_t kReusedSlotDcbaaSentinel = 0x12340000u;
  dcbaa[child_slot] = kReusedSlotDcbaaSentinel;
  EXPECT_OK(xhci->RunSynchronously(0, xhci->DisableSlotCommand(*child_state, child_state)));
  EXPECT_EQ(dcbaa[child_slot], kReusedSlotDcbaaSentinel);
  dcbaa[child_slot] = 0;

  // Reinitializing a completed DeviceState resets all slot-disable state, including slot_disabled_.
  {
    fbl::AutoLock _(&child_state->transaction_lock());
    EXPECT_TRUE(child_state->slot_disabled_.signaled());
    child_state->SetDeviceInformation(child_slot, 2, std::nullopt);
    EXPECT_FALSE(child_state->IsDisconnecting());
    EXPECT_FALSE(child_state->SlotDisableStatusLocked().has_value());
    EXPECT_FALSE(child_state->slot_disabled_.signaled());
    EXPECT_FALSE(child_state->ExchangeSlotDisableStarted());
  }
}

TEST_F(XhciMmioHarness, SetsMultiTtAndGuardsTtFieldsInSlotContext) {
  UsbXhci* xhci = driver_test().driver();
  CommandCompletionEvent command_event;
  command_event.set_CompletionCode(CommandCompletionEvent::Success);
  std::optional<HubInfo> captured_hub_info;
  SetDoorbellListener([&](uint8_t doorbell, uint8_t target) {
    if (doorbell == 0) {
      auto pending = xhci->GetCommandRing()->TakePendingTRBs();
      for (auto& cmd : pending) {
        if (cmd.completer.has_value()) {
          cmd.completer->complete_ok(&command_event);
        }
      }
    } else if (target == 1) {
      if (auto state = xhci->GetDeviceState(doorbell - 1)) {
        fbl::DoublyLinkedList<std::unique_ptr<TRBContext>> pending;
        {
          fbl::AutoLock _(&state->transaction_lock());
          pending = state->GetEndpoint().transfer_ring().TakePendingTRBs();
        }
        for (auto& trb : pending) {
          if (trb.request.has_value()) {
            std::get<Request>(*trb.request).Complete(ZX_OK, 0);
          }
        }
      }
    }
  });
  SetEnumerateDeviceListener(
      [&captured_hub_info](uint8_t /*port*/, const std::optional<HubInfo>& hub_info) {
        captured_hub_info = hub_info;
      });
  auto clear_listeners = fit::defer([this] {
    SetEnumerateDeviceListener(nullptr);
    SetDoorbellListener([](uint8_t, uint8_t) {});
  });

  auto slot_ctx_of = [xhci](const dma_buffer::PagedBuffer* input_context) -> SlotContext* {
    return reinterpret_cast<SlotContext*>(static_cast<unsigned char*>(input_context->virt()) +
                                          xhci->slot_size_bytes());
  };
  auto hub_device_added_info = [&](uint32_t parent_device_id, uint32_t port,
                                   usb_speed_t speed) -> std::optional<HubInfo> {
    captured_hub_info.reset();
    EXPECT_OK(xhci->UsbHciHubDeviceAdded(parent_device_id, port, speed));
    return std::exchange(captured_hub_info, std::nullopt);
  };

  FakeUsbDevice hub_dev = ConnectDevice(1, USB_SPEED_HIGH);
  auto hub_state = xhci->GetDeviceState(hub_dev.device_id);
  ASSERT_TRUE(hub_state);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    slot_ctx_of(hub_state->GetInputContext().get())->set_CONTEXT_ENTRIES(0);
  }

  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 4;

  // Failed ConfigureHub from an unconfigured hub rolls back CONTEXT_ENTRIES, MULTI_TT, HUB, and
  // hub_ state.
  command_event.set_CompletionCode(CommandCompletionEvent::ParameterError);
  EXPECT_EQ(xhci->UsbHciConfigureHub(hub_dev.device_id, USB_SPEED_HIGH, &hub_desc, true),
            ZX_ERR_IO);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    EXPECT_FALSE(hub_state->GetHubLocked().has_value());
    auto* slot_ctx = slot_ctx_of(hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 0u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
    EXPECT_EQ(slot_ctx->HUB(), 0u);
  }
  command_event.set_CompletionCode(CommandCompletionEvent::Success);

  ASSERT_OK(xhci->UsbHciConfigureHub(hub_dev.device_id, USB_SPEED_HIGH, &hub_desc, true));
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 1u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
    slot_ctx->set_CONTEXT_ENTRIES(4);
  }

  // Re-configuring when hub interrupt IN endpoint already raised CONTEXT_ENTRIES to 4 preserves 4.
  ASSERT_OK(xhci->UsbHciConfigureHub(hub_dev.device_id, USB_SPEED_HIGH, &hub_desc, true));
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 4u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
  }

  // Failed re-configure from an already-configured multi_tt = true hub (requesting multi_tt =
  // false) rolls back MULTI_TT = 1, HUB = 1, CONTEXT_ENTRIES = 4, and hub_->multi_tt = true.
  command_event.set_CompletionCode(CommandCompletionEvent::ParameterError);
  EXPECT_EQ(xhci->UsbHciConfigureHub(hub_dev.device_id, USB_SPEED_HIGH, &hub_desc, false),
            ZX_ERR_IO);
  {
    fbl::AutoLock _(&hub_state->transaction_lock());
    const auto& hub = hub_state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    EXPECT_TRUE(hub->multi_tt);
    auto* slot_ctx = slot_ctx_of(hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->CONTEXT_ENTRIES(), 4u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
    EXPECT_EQ(slot_ctx->HUB(), 1u);
  }
  command_event.set_CompletionCode(CommandCompletionEvent::Success);

  // ConfigureHub with multi_tt = true on Full-Speed and SuperSpeed root hubs clears MULTI_TT.
  FakeUsbDevice root_fs_hub_dev = ConnectDevice(2, USB_SPEED_FULL);
  ASSERT_OK(xhci->UsbHciConfigureHub(root_fs_hub_dev.device_id, USB_SPEED_FULL, &hub_desc, true));
  {
    auto root_fs_hub_state = xhci->GetDeviceState(root_fs_hub_dev.device_id);
    ASSERT_TRUE(root_fs_hub_state);
    fbl::AutoLock _(&root_fs_hub_state->transaction_lock());
    const auto& hub = root_fs_hub_state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    EXPECT_FALSE(hub->multi_tt);
    auto* slot_ctx = slot_ctx_of(root_fs_hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->HUB(), 1u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  FakeUsbDevice root_ss_hub_dev = ConnectDevice(3, USB_SPEED_SUPER);
  ASSERT_OK(xhci->UsbHciConfigureHub(root_ss_hub_dev.device_id, USB_SPEED_SUPER, &hub_desc, true));
  {
    auto root_ss_hub_state = xhci->GetDeviceState(root_ss_hub_dev.device_id);
    ASSERT_TRUE(root_ss_hub_state);
    fbl::AutoLock _(&root_ss_hub_state->transaction_lock());
    const auto& hub = root_ss_hub_state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    EXPECT_FALSE(hub->multi_tt);
    auto* slot_ctx = slot_ctx_of(root_ss_hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->HUB(), 1u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  // Single-TT High-Speed root hub keeps multi_tt = false and SlotContext MULTI_TT = 0.
  FakeUsbDevice stt_hub_dev = ConnectDevice(4, USB_SPEED_HIGH);
  ASSERT_OK(xhci->UsbHciConfigureHub(stt_hub_dev.device_id, USB_SPEED_HIGH, &hub_desc, false));
  auto stt_hub_state = xhci->GetDeviceState(stt_hub_dev.device_id);
  ASSERT_TRUE(stt_hub_state);
  {
    fbl::AutoLock _(&stt_hub_state->transaction_lock());
    const auto& hub = stt_hub_state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    EXPECT_FALSE(hub->multi_tt);
    auto* slot_ctx = slot_ctx_of(stt_hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->HUB(), 1u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  // Low-Speed child behind Multi-TT High-Speed hub propagates multi_tt = true via
  // UsbHciHubDeviceAddedAsync and gets TT slot/port and MULTI_TT = 1.
  std::optional<HubInfo> ls_info = hub_device_added_info(hub_dev.device_id, 3, USB_SPEED_LOW);
  ASSERT_TRUE(ls_info.has_value());
  EXPECT_TRUE(ls_info->multi_tt);
  const uint8_t ls_slot = AllocateSlot();
  xhci->SetDeviceInformation(ls_slot, 3, ls_info);
  ASSERT_OK(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(ls_slot, 3, ls_info, false)));
  {
    auto ls_state = xhci->GetDeviceState(ls_slot - 1);
    ASSERT_TRUE(ls_state);
    fbl::AutoLock _(&ls_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(ls_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->PARENT_HUB_SLOT_ID(), hub_state->GetSlot());
    EXPECT_EQ(slot_ctx->PARENT_PORT_NUMBER(), 3u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
  }

  // Full-Speed non-hub child behind Multi-TT High-Speed hub gets TT slot/port and MULTI_TT = 1.
  std::optional<HubInfo> fs_info = hub_device_added_info(hub_dev.device_id, 4, USB_SPEED_FULL);
  ASSERT_TRUE(fs_info.has_value());
  EXPECT_TRUE(fs_info->multi_tt);
  const uint8_t fs_slot = AllocateSlot();
  xhci->SetDeviceInformation(fs_slot, 4, fs_info);
  ASSERT_OK(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(fs_slot, 4, fs_info, false)));
  {
    auto fs_state = xhci->GetDeviceState(fs_slot - 1);
    ASSERT_TRUE(fs_state);
    fbl::AutoLock _(&fs_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(fs_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->PARENT_HUB_SLOT_ID(), hub_state->GetSlot());
    EXPECT_EQ(slot_ctx->PARENT_PORT_NUMBER(), 4u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
  }

  // Low-Speed child behind Single-TT High-Speed hub propagates multi_tt = false and gets TT
  // slot/port and MULTI_TT = 0.
  std::optional<HubInfo> stt_ls_info =
      hub_device_added_info(stt_hub_dev.device_id, 2, USB_SPEED_LOW);
  ASSERT_TRUE(stt_ls_info.has_value());
  EXPECT_FALSE(stt_ls_info->multi_tt);
  const uint8_t stt_ls_slot = AllocateSlot();
  xhci->SetDeviceInformation(stt_ls_slot, 2, stt_ls_info);
  ASSERT_OK(
      xhci->RunSynchronously(0, xhci->AddressDeviceCommand(stt_ls_slot, 2, stt_ls_info, false)));
  {
    auto stt_ls_state = xhci->GetDeviceState(stt_ls_slot - 1);
    ASSERT_TRUE(stt_ls_state);
    fbl::AutoLock _(&stt_ls_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(stt_ls_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->PARENT_HUB_SLOT_ID(), stt_hub_state->GetSlot());
    EXPECT_EQ(slot_ctx->PARENT_PORT_NUMBER(), 2u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  // Cascaded Full-Speed hub behind Multi-TT High-Speed hub clears SlotContext MULTI_TT to 0
  // when configured with HUB = 1 (even if multi_tt = true is passed), while preserving the
  // ancestor TT's multi_tt flag in tt_info for downstream tier-3 Low-/Full-Speed children.
  std::optional<HubInfo> fs_hub_info = hub_device_added_info(hub_dev.device_id, 1, USB_SPEED_FULL);
  ASSERT_TRUE(fs_hub_info.has_value());
  const uint8_t fs_hub_slot = AllocateSlot();
  xhci->SetDeviceInformation(fs_hub_slot, 1, fs_hub_info);
  ASSERT_OK(
      xhci->RunSynchronously(0, xhci->AddressDeviceCommand(fs_hub_slot, 1, fs_hub_info, false)));
  ASSERT_OK(xhci->UsbHciConfigureHub(fs_hub_slot - 1, USB_SPEED_FULL, &hub_desc, true));
  auto fs_hub_state = xhci->GetDeviceState(fs_hub_slot - 1);
  ASSERT_TRUE(fs_hub_state);
  {
    fbl::AutoLock _(&fs_hub_state->transaction_lock());
    const auto& hub = fs_hub_state->GetHubLocked();
    ASSERT_TRUE(hub.has_value());
    EXPECT_FALSE(hub->multi_tt);
    const auto& tt_info = hub->tt_info;
    ASSERT_TRUE(tt_info.has_value());
    EXPECT_EQ(tt_info->tt_slot_id, hub_state->GetSlot());
    EXPECT_EQ(tt_info->tt_port_number, 1u);
    EXPECT_TRUE(tt_info->multi_tt);
    auto* slot_ctx = slot_ctx_of(fs_hub_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->HUB(), 1u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  // Tier-3 Low-Speed child behind cascaded Full-Speed hub inherits ancestor HS hub's TT slot,
  // port, and MULTI_TT = 1 via UsbHciHubDeviceAddedAsync propagating tt_info even though
  // fs_hub_state->GetHubLocked()->multi_tt is false.
  std::optional<HubInfo> tier3_ls_info = hub_device_added_info(fs_hub_slot - 1, 2, USB_SPEED_LOW);
  ASSERT_TRUE(tier3_ls_info.has_value());
  EXPECT_FALSE(tier3_ls_info->multi_tt);
  ASSERT_TRUE(tier3_ls_info->tt_info.has_value());
  EXPECT_EQ(tier3_ls_info->tt_info->tt_slot_id, hub_state->GetSlot());
  EXPECT_EQ(tier3_ls_info->tt_info->tt_port_number, 1u);
  EXPECT_TRUE(tier3_ls_info->tt_info->multi_tt);
  const uint8_t tier3_ls_slot = AllocateSlot();
  xhci->SetDeviceInformation(tier3_ls_slot, 2, tier3_ls_info);
  ASSERT_OK(xhci->RunSynchronously(
      0, xhci->AddressDeviceCommand(tier3_ls_slot, 2, tier3_ls_info, false)));
  {
    auto tier3_ls_state = xhci->GetDeviceState(tier3_ls_slot - 1);
    ASSERT_TRUE(tier3_ls_state);
    fbl::AutoLock _(&tier3_ls_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(tier3_ls_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->PARENT_HUB_SLOT_ID(), hub_state->GetSlot());
    EXPECT_EQ(slot_ctx->PARENT_PORT_NUMBER(), 1u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 1u);
  }

  // High-Speed child behind High-Speed hub must NOT inherit TT fields even if tt_info is present.
  std::optional<HubInfo> hs_info = hub_device_added_info(hub_dev.device_id, 2, USB_SPEED_HIGH);
  ASSERT_TRUE(hs_info.has_value());
  EXPECT_FALSE(hs_info->tt_info.has_value());
  hs_info->tt_info =
      tt_info_t{.tt_slot_id = hub_state->GetSlot(), .tt_port_number = 2, .multi_tt = true};
  const uint8_t hs_slot = AllocateSlot();
  xhci->SetDeviceInformation(hs_slot, 2, hs_info);
  ASSERT_OK(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(hs_slot, 2, hs_info, false)));
  {
    auto hs_state = xhci->GetDeviceState(hs_slot - 1);
    ASSERT_TRUE(hs_state);
    fbl::AutoLock _(&hs_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(hs_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->PARENT_HUB_SLOT_ID(), 0u);
    EXPECT_EQ(slot_ctx->PARENT_PORT_NUMBER(), 0u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }

  // SuperSpeed child must NOT inherit TT fields even if tt_info is present.
  std::optional<HubInfo> ss_info =
      hub_device_added_info(root_ss_hub_dev.device_id, 1, USB_SPEED_SUPER);
  ASSERT_TRUE(ss_info.has_value());
  EXPECT_FALSE(ss_info->multi_tt);
  EXPECT_FALSE(ss_info->tt_info.has_value());
  ss_info->multi_tt = true;
  ss_info->tt_info =
      tt_info_t{.tt_slot_id = hub_state->GetSlot(), .tt_port_number = 1, .multi_tt = true};
  const uint8_t ss_slot = AllocateSlot();
  xhci->SetDeviceInformation(ss_slot, 1, ss_info);
  ASSERT_OK(xhci->RunSynchronously(0, xhci->AddressDeviceCommand(ss_slot, 1, ss_info, false)));
  {
    auto ss_state = xhci->GetDeviceState(ss_slot - 1);
    ASSERT_TRUE(ss_state);
    fbl::AutoLock _(&ss_state->transaction_lock());
    auto* slot_ctx = slot_ctx_of(ss_state->GetInputContext().get());
    EXPECT_EQ(slot_ctx->PARENT_HUB_SLOT_ID(), 0u);
    EXPECT_EQ(slot_ctx->PARENT_PORT_NUMBER(), 0u);
    EXPECT_EQ(slot_ctx->MULTI_TT(), 0u);
  }
}

TEST(XhciStartFailureTest, StartFailureCleansUpDdkInteractionDispatcher) {
  // Without pdev or pci in the incoming namespace (enforced via SetServiceValidator),
  // UsbXhci::Start() creates ddk_interaction_dispatcher_ and then returns ZX_ERR_IO_INVALID. DFv2
  // destroys UsbXhci via ~UsbXhci() without calling Stop(); ~UsbXhci() must cleanly shut down
  // ddk_interaction_dispatcher_ before ddk_interaction_shutdown_ is destroyed.
  fdf_testing::ForegroundDriverTest<EmptyTestConfig> driver_test;
  driver_test.SetServiceValidator(true);
  zx::result<> start_result =
      driver_test.StartDriverWithCustomStartArgs([](fdf::DriverStartArgs& args) {
        xhci_config::Config fake_config;
        fake_config.enable_suspend() = false;
        args.config(fake_config.ToVmo());
      });
  EXPECT_STATUS(start_result.status_value(), ZX_ERR_IO_INVALID);
  driver_test.ShutdownAndDestroyDriver();
}

TEST_F(XhciMmioHarness, DeviceOnlineSkipsAddDeviceWhenBusCleared) {
  UsbXhci* xhci = driver_test().driver();
  auto doorbell = InstallCommandDoorbellListener();

  TestHub hub = ConnectConfiguredHub(1);
  ASSERT_TRUE(hub.state);
  const uint8_t hub_child_slot = AttachHubChild(hub.state, 1, 1);
  const uint8_t root_slot = AddressRootSlot(2);

  // Clear the bus interface as on usb-bus unbind; both hub-attached and root-hub DeviceOnline()
  // callbacks must observe !bus.is_valid() and skip bus.AddDevice() without crashing.
  xhci->UsbHciSetBusInterface(nullptr);
  ASSERT_OK(xhci->DeviceOnline(hub_child_slot, 1, USB_SPEED_FULL));
  ASSERT_OK(xhci->DeviceOnline(root_slot, 2, USB_SPEED_HIGH));
  ASSERT_OK(xhci->FlushDdkCallbacksForTesting());

  EXPECT_EQ(bus_.devices().count(hub_child_slot - 1u), 0u);
  EXPECT_EQ(bus_.devices().count(root_slot - 1u), 0u);
}

TEST_F(XhciMmioHarness, HubDeviceRemovedSerializesAndSkipsUnaddedBusDevices) {
  UsbXhci* xhci = driver_test().driver();
  auto doorbell = InstallCommandDoorbellListener();

  TestHub hub = ConnectConfiguredHub(1);
  ASSERT_TRUE(hub.state);
  bus_.set_strict_remove(true);

  // (a) UsbHciHubDeviceRemoved() runs while DeviceOnline()'s PostCallback is queued behind a
  // blocker: DeviceOnline() sees IsDisconnecting() and skips bus.AddDevice(), and
  // UsbHciHubDeviceRemoved() serializes behind it, skips bus.RemoveDevice(), and returns ZX_OK.
  {
    const uint8_t blocker_slot = AllocateSlot();
    xhci->SetDeviceInformation(blocker_slot, 2, std::nullopt);
    const uint8_t queued_child_slot = AttachHubChild(hub.state, 1, 1);
    AddDeviceBlocker blocker(&bus_, blocker_slot - 1u, ZX_OK);
    ASSERT_OK(xhci->DeviceOnline(blocker_slot, 2, USB_SPEED_HIGH));
    ASSERT_OK(blocker.WaitEntered());
    ASSERT_OK(xhci->DeviceOnline(queued_child_slot, 1, USB_SPEED_FULL));
    bus_.reset_remove_device_calls();

    HubRemoveOp queued_remove(xhci, hub.id, 1, blocker.release_completion());
    ASSERT_OK(queued_remove.WaitEnqueued());
    EXPECT_EQ(xhci->GetDeviceState(queued_child_slot - 1u), nullptr);
    EXPECT_FALSE(queued_remove.IsDone());
    blocker.Release();
    ASSERT_OK(queued_remove.WaitAndJoin());
    EXPECT_OK(queued_remove.status());
    EXPECT_EQ(bus_.remove_device_calls(), 0u);
    EXPECT_EQ(bus_.devices().count(queued_child_slot - 1u), 0u);
  }

  // (b) UsbHciHubDeviceRemoved() runs while bus.AddDevice() for the child is already in-flight on
  // ddk_interaction_dispatcher_: it waits for bus.AddDevice() to finish and then removes the newly
  // registered device without returning ZX_ERR_BAD_STATE or leaving an orphan in usb-bus.
  {
    const uint8_t inflight_child_slot = AttachHubChild(hub.state, 2, 1);
    const uint32_t inflight_child_id = inflight_child_slot - 1u;
    AddDeviceBlocker blocker(&bus_, inflight_child_id, std::nullopt);
    ASSERT_OK(xhci->DeviceOnline(inflight_child_slot, 2, USB_SPEED_FULL));
    ASSERT_OK(blocker.WaitEntered());
    bus_.reset_remove_device_calls();

    HubRemoveOp inflight_remove(xhci, hub.id, 2, blocker.release_completion());
    ASSERT_OK(inflight_remove.WaitEnqueued());
    EXPECT_EQ(xhci->GetDeviceState(inflight_child_id), nullptr);
    EXPECT_FALSE(inflight_remove.IsDone());
    blocker.Release();
    ASSERT_OK(inflight_remove.WaitAndJoin());
    EXPECT_OK(inflight_remove.status());
    EXPECT_EQ(bus_.remove_device_calls(), 1u);
    EXPECT_EQ(bus_.devices().count(inflight_child_id), 0u);
  }

  // (c) Nested hub child (added to bus_) with an unadded downstream grandchild: only the added
  // nested hub is passed to bus.RemoveDevice().
  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 4;
  const uint8_t nested_hub_slot =
      AttachHubChild(hub.state, 3, 1, /*bring_online=*/true, USB_SPEED_HIGH);
  const uint32_t nested_hub_id = nested_hub_slot - 1u;
  fbl::RefPtr<DeviceState> nested_hub_state = xhci->GetDeviceState(nested_hub_id);
  ASSERT_TRUE(nested_hub_state);
  ASSERT_OK(xhci->UsbHciConfigureHub(nested_hub_id, USB_SPEED_HIGH, &hub_desc, true));
  const uint8_t unadded_grandchild_slot = AttachHubChild(nested_hub_state, 1, 1);
  bus_.reset_remove_device_calls();
  EXPECT_OK(xhci->UsbHciHubDeviceRemoved(hub.id, 3));
  EXPECT_EQ(bus_.remove_device_calls(), 1u);
  EXPECT_EQ(bus_.devices().count(nested_hub_id), 0u);
  EXPECT_EQ(xhci->GetDeviceState(nested_hub_id), nullptr);
  EXPECT_EQ(xhci->GetDeviceState(unadded_grandchild_slot - 1u), nullptr);

  // (d) RemoveSubtreeFromBusOnDdkDispatcher() error paths on registered devices: a non-ZX_OK
  // bus.RemoveDevice() on a registered downstream child logs and still removes the parent hub,
  // and a non-ZX_OK bus.RemoveDevice() on the root target propagates that status to the caller.
  const uint8_t err_hub_slot =
      AttachHubChild(hub.state, 4, 1, /*bring_online=*/true, USB_SPEED_HIGH);
  const uint32_t err_hub_id = err_hub_slot - 1u;
  fbl::RefPtr<DeviceState> err_hub_state = xhci->GetDeviceState(err_hub_id);
  ASSERT_TRUE(err_hub_state);
  ASSERT_OK(xhci->UsbHciConfigureHub(err_hub_id, USB_SPEED_HIGH, &hub_desc, true));
  const uint8_t err_grandchild_slot = AttachHubChild(err_hub_state, 1, 1, /*bring_online=*/true);
  const uint32_t err_grandchild_id = err_grandchild_slot - 1u;
  ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
  bus_.reset_remove_device_calls();
  std::vector<uint32_t> removed_order;
  fbl::Mutex removed_order_lock;
  bus_.set_remove_device_hook([&removed_order, &removed_order_lock, err_grandchild_id,
                               err_hub_id](uint32_t device_id) -> std::optional<zx_status_t> {
    {
      fbl::AutoLock _(&removed_order_lock);
      removed_order.push_back(device_id);
    }
    if (device_id == err_grandchild_id) {
      return ZX_ERR_INTERNAL;
    }
    if (device_id == err_hub_id) {
      return ZX_ERR_IO;
    }
    return std::nullopt;
  });
  EXPECT_STATUS(xhci->UsbHciHubDeviceRemoved(hub.id, 4), ZX_ERR_IO);
  bus_.clear_remove_device_hook();
  EXPECT_EQ(bus_.remove_device_calls(), 2u);
  {
    fbl::AutoLock _(&removed_order_lock);
    ASSERT_EQ(removed_order.size(), 2u);
    EXPECT_EQ(removed_order[0], err_grandchild_id);
    EXPECT_EQ(removed_order[1], err_hub_id);
  }
  EXPECT_EQ(xhci->GetDeviceState(err_hub_id), nullptr);
  EXPECT_EQ(xhci->GetDeviceState(err_grandchild_id), nullptr);
}

TEST_F(XhciMmioHarness, HubDeviceRemovedReentrantOnDdkDispatcher) {
  UsbXhci* xhci = driver_test().driver();
  auto doorbell = InstallCommandDoorbellListener();

  TestHub hub = ConnectConfiguredHub(2);
  ASSERT_TRUE(hub.state);
  bus_.set_strict_remove(true);
  const uint8_t child_a_slot = AttachHubChild(hub.state, 1, 2, /*bring_online=*/true);
  const uint8_t child_b_slot = AttachHubChild(hub.state, 2, 2);
  const uint32_t child_b_id = child_b_slot - 1u;

  struct ReentrantState {
    zx_status_t status = ZX_ERR_INTERNAL;
    libsync::Completion done;
  };
  auto reentrant = std::make_shared<ReentrantState>();
  bus_.reset_remove_device_calls();
  // Invoke UsbHciHubDeviceRemoved() directly from ddk_interaction_dispatcher_ (inside
  // bus.AddDevice() for child_b) to verify the inline fast path avoids self-deadlock.
  bus_.set_add_device_hook([xhci, hub_id = hub.id, child_b_id, reentrant](
                               uint32_t device_id, uint32_t /*hub_id*/,
                               usb_speed_t /*speed*/) -> std::optional<zx_status_t> {
    if (device_id == child_b_id) {
      reentrant->status = xhci->UsbHciHubDeviceRemoved(hub_id, 1);
      reentrant->done.Signal();
    }
    return std::nullopt;
  });
  ASSERT_OK(xhci->DeviceOnline(child_b_slot, 2, USB_SPEED_FULL));
  ASSERT_OK(reentrant->done.Wait(zx::sec(5)));
  ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
  bus_.clear_add_device_hook();

  EXPECT_OK(reentrant->status);
  EXPECT_EQ(bus_.remove_device_calls(), 1u);
  EXPECT_EQ(bus_.devices().count(child_a_slot - 1u), 0u);
  EXPECT_EQ(bus_.devices().count(child_b_id), 1u);
  EXPECT_EQ(xhci->GetDeviceState(child_a_slot - 1u), nullptr);

  // Reentrant UsbHciHubDeviceRemoved() for the same child currently inside bus.AddDevice():
  // RemoveSubtreeFromBusOnDdkDispatcher() transitions the state to kRemoved and skips
  // bus.RemoveDevice() before bus.AddDevice() returns, and DeviceOnline() then removes the newly
  // registered device instead of leaving an orphan in usb-bus.
  const uint8_t child_c_slot = AttachHubChild(hub.state, 3, 2);
  const uint32_t child_c_id = child_c_slot - 1u;
  auto reentrant_same = std::make_shared<ReentrantState>();
  bus_.reset_remove_device_calls();
  bus_.set_add_device_hook([xhci, hub_id = hub.id, child_c_id, reentrant_same](
                               uint32_t device_id, uint32_t /*hub_id*/,
                               usb_speed_t /*speed*/) -> std::optional<zx_status_t> {
    if (device_id == child_c_id) {
      reentrant_same->status = xhci->UsbHciHubDeviceRemoved(hub_id, 3);
      reentrant_same->done.Signal();
    }
    return std::nullopt;
  });
  ASSERT_OK(xhci->DeviceOnline(child_c_slot, 3, USB_SPEED_FULL));
  ASSERT_OK(reentrant_same->done.Wait(zx::sec(5)));
  ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
  bus_.clear_add_device_hook();

  EXPECT_OK(reentrant_same->status);
  EXPECT_EQ(bus_.remove_device_calls(), 1u);
  EXPECT_EQ(bus_.devices().count(child_c_id), 0u);
  EXPECT_EQ(xhci->GetDeviceState(child_c_id), nullptr);
}

TEST_F(XhciMmioHarness, PartialSubtreeDrainRemovesDownstreamAndPropagatesRootError) {
  UsbXhci* xhci = driver_test().driver();
  auto fail_disable_slot_id = std::make_shared<std::atomic<uint8_t>>(0);
  auto doorbell = InstallCommandDoorbellListener(fail_disable_slot_id);
  bus_.set_strict_remove(true);

  usb_hub_descriptor_t hub_desc = {};
  hub_desc.b_nbr_ports = 4;

  // (a) UsbHciHubDeviceRemoved(): downstream grandchild DisableSlot succeeds while the nested hub
  // root DisableSlot fails on a running controller. The downstream child is drained and removed
  // from bus_, while the root hub remains pinned in device_state_ and bus_ and root_error is
  // propagated.
  {
    TestHub parent_hub = ConnectConfiguredHub(1);
    ASSERT_TRUE(parent_hub.state);
    const uint8_t nested_hub_slot =
        AttachHubChild(parent_hub.state, 1, 1, /*bring_online=*/true, USB_SPEED_HIGH);
    const uint32_t nested_hub_id = nested_hub_slot - 1u;
    fbl::RefPtr<DeviceState> nested_hub_state = xhci->GetDeviceState(nested_hub_id);
    ASSERT_TRUE(nested_hub_state);
    ASSERT_OK(xhci->UsbHciConfigureHub(nested_hub_id, USB_SPEED_HIGH, &hub_desc, true));
    const uint8_t grandchild_slot = AttachHubChild(nested_hub_state, 1, 1, /*bring_online=*/true);
    const uint32_t grandchild_id = grandchild_slot - 1u;
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    ASSERT_EQ(bus_.devices().count(nested_hub_id), 1u);
    ASSERT_EQ(bus_.devices().count(grandchild_id), 1u);

    fail_disable_slot_id->store(nested_hub_slot, std::memory_order_release);
    bus_.reset_remove_device_calls();
    EXPECT_STATUS(xhci->UsbHciHubDeviceRemoved(parent_hub.id, 1), ZX_ERR_IO);
    fail_disable_slot_id->store(0, std::memory_order_release);

    EXPECT_EQ(bus_.remove_device_calls(), 1u);
    EXPECT_EQ(bus_.devices().count(grandchild_id), 0u);
    EXPECT_EQ(xhci->GetDeviceState(grandchild_id), nullptr);
    EXPECT_EQ(bus_.devices().count(nested_hub_id), 1u);
    EXPECT_NE(xhci->GetDeviceState(nested_hub_id), nullptr);
  }

  // (b) DeviceOffline(): same partial subtree drain when the root hub's DisableSlot fails while
  // its registered downstream child succeeds.
  {
    TestHub offline_hub = ConnectConfiguredHub(2);
    ASSERT_TRUE(offline_hub.state);
    const uint8_t child_slot = AttachHubChild(offline_hub.state, 1, 2, /*bring_online=*/true);
    const uint32_t child_id = child_slot - 1u;
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    ASSERT_EQ(bus_.devices().count(offline_hub.id), 1u);
    ASSERT_EQ(bus_.devices().count(child_id), 1u);

    fail_disable_slot_id->store(offline_hub.slot, std::memory_order_release);
    bus_.reset_remove_device_calls();
    auto offline = ScheduleDeviceOffline(offline_hub.slot);
    xhci->RunUntilIdle();
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    xhci->RunUntilIdle();
    ASSERT_OK(offline->completion.Wait(zx::sec(5)));
    fail_disable_slot_id->store(0, std::memory_order_release);

    EXPECT_TRUE(offline->is_error.load(std::memory_order_acquire));
    EXPECT_STATUS(offline->status.load(std::memory_order_acquire), ZX_ERR_IO);
    EXPECT_EQ(bus_.remove_device_calls(), 1u);
    EXPECT_EQ(bus_.devices().count(child_id), 0u);
    EXPECT_EQ(xhci->GetDeviceState(child_id), nullptr);
    EXPECT_EQ(bus_.devices().count(offline_hub.id), 1u);
    EXPECT_NE(xhci->GetDeviceState(offline_hub.id), nullptr);
  }
}

TEST_F(XhciMmioHarness, DeviceOfflineSkipsUnaddedBusDevicesAndResolvesOnStop) {
  UsbXhci* xhci = driver_test().driver();
  auto doorbell = InstallCommandDoorbellListener();

  // (a) DeviceOffline() runs while DeviceOnline()'s PostCallback is queued behind a blocker: the
  // online callback sees IsDisconnecting() and skips bus.AddDevice(), and DeviceOffline() skips
  // bus.RemoveDevice() under strict_remove.
  bus_.set_strict_remove(true);
  {
    const uint8_t skip_blocker_slot = AllocateSlot();
    xhci->SetDeviceInformation(skip_blocker_slot, 2, std::nullopt);
    const uint8_t skip_offline_slot = AddressRootSlot(2);
    AddDeviceBlocker blocker(&bus_, skip_blocker_slot - 1u, ZX_OK);
    ASSERT_OK(xhci->DeviceOnline(skip_blocker_slot, 2, USB_SPEED_HIGH));
    ASSERT_OK(blocker.WaitEntered());
    ASSERT_OK(xhci->DeviceOnline(skip_offline_slot, 2, USB_SPEED_HIGH));
    bus_.reset_remove_device_calls();
    auto skip_offline = ScheduleDeviceOffline(skip_offline_slot);
    xhci->RunUntilIdle();
    blocker.Release();
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    xhci->RunUntilIdle();
    ASSERT_OK(skip_offline->completion.Wait(zx::sec(5)));
    EXPECT_OK(skip_offline->status.load(std::memory_order_acquire));
    EXPECT_EQ(bus_.remove_device_calls(), 0u);
    EXPECT_FALSE(xhci->IsHaltedOrStopping());
  }

  // (b) Steady-state bus.AddDevice() failure while running_ == true leaves
  // bus_registration_state_ at kNotAdded so a subsequent DeviceOffline() skips bus.RemoveDevice()
  // under strict_remove.
  {
    const uint8_t fail_add_slot = AddressRootSlot(3);
    bus_.set_add_device_hook(
        [target_id = fail_add_slot - 1u](uint32_t device_id, uint32_t /*hub_id*/,
                                         usb_speed_t /*speed*/) -> std::optional<zx_status_t> {
          return device_id == target_id ? std::optional<zx_status_t>(ZX_ERR_IO) : std::nullopt;
        });
    ASSERT_OK(xhci->DeviceOnline(fail_add_slot, 3, USB_SPEED_HIGH));
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    bus_.clear_add_device_hook();
    EXPECT_EQ(bus_.devices().count(fail_add_slot - 1u), 0u);
    bus_.reset_remove_device_calls();
    auto fail_add_offline = ScheduleDeviceOffline(fail_add_slot);
    xhci->RunUntilIdle();
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    xhci->RunUntilIdle();
    ASSERT_OK(fail_add_offline->completion.Wait(zx::sec(5)));
    EXPECT_FALSE(fail_add_offline->is_error.load(std::memory_order_acquire));
    EXPECT_OK(fail_add_offline->status.load(std::memory_order_acquire));
    EXPECT_EQ(bus_.remove_device_calls(), 0u);
    EXPECT_EQ(xhci->GetDeviceState(fail_add_slot - 1u), nullptr);
  }

  // (c) Hub-attached child whose bus.AddDevice() was never called is not passed to
  // bus.RemoveDevice() when the parent hub goes offline (only the parent hub is removed).
  TestHub strict_hub = ConnectConfiguredHub(4);
  ASSERT_TRUE(strict_hub.state);
  (void)AttachHubChild(strict_hub.state, 1, 4);
  bus_.reset_remove_device_calls();
  RunDeviceOffline(strict_hub.slot);
  EXPECT_EQ(bus_.remove_device_calls(), 1u);
  EXPECT_FALSE(xhci->IsHaltedOrStopping());
  bus_.set_strict_remove(false);

  // (d) Unexpected controller halt (USBSTS.HCHalted == 1 while running_ == true): DeviceOffline()
  // still notifies usb-bus via bus.RemoveDevice() rather than stranding the UsbDevice child.
  {
    FakeUsbDevice halted_dev = ConnectDevice(3, USB_SPEED_HIGH);
    const uint8_t halted_slot = static_cast<uint8_t>(halted_dev.device_id + 1u);
    ASSERT_EQ(bus_.devices().count(halted_dev.device_id), 1u);
    constexpr uint8_t kCapLength = 0x1c;
    driver_test().RunInEnvironmentTypeContext([](XhciTestEnvironment& env) {
      fdf::MmioBuffer mmio = env.fake_device().mmio();
      USBCMD::Get(kCapLength).ReadFrom(&mmio).set_ENABLE(0).WriteTo(&mmio);
    });
    auto restore_enable = fit::defer([this] {
      driver_test().RunInEnvironmentTypeContext([](XhciTestEnvironment& env) {
        fdf::MmioBuffer mmio = env.fake_device().mmio();
        USBCMD::Get(kCapLength).ReadFrom(&mmio).set_ENABLE(1).WriteTo(&mmio);
      });
    });
    ASSERT_TRUE(xhci->Running());
    ASSERT_TRUE(xhci->IsControllerHalted());
    bus_.reset_remove_device_calls();
    RunDeviceOffline(halted_slot);
    EXPECT_EQ(bus_.remove_device_calls(), 1u);
    EXPECT_EQ(bus_.devices().count(halted_dev.device_id), 0u);
    EXPECT_EQ(xhci->GetDeviceState(halted_dev.device_id), nullptr);
  }

  // (e) StopDriver() while ddk_interaction_dispatcher_ is blocked in bus.AddDevice() with queued
  // DeviceOnline(), DeviceOffline(), and UsbHciHubDeviceRemoved() callbacks: queued DeviceOnline()
  // skips bus.AddDevice(), UsbHciHubDeviceRemoved() wakes cleanly with ZX_OK, and queued
  // DeviceOffline() promises resolve cleanly instead of abandoning downstream continuations.
  TestHub stop_hub = ConnectConfiguredHub(1);
  ASSERT_TRUE(stop_hub.state);
  const uint8_t hub_child_slot = AttachHubChild(stop_hub.state, 1, 1);
  const uint8_t blocked_slot = AddressRootSlot(2);
  const uint8_t queued_online_slot = AddressRootSlot(3);
  const uint8_t queued_offline_slot = AddressRootSlot(4);

  AddDeviceBlocker blocker(&bus_, blocked_slot - 1u, ZX_ERR_IO);
  ASSERT_OK(xhci->DeviceOnline(blocked_slot, 2, USB_SPEED_HIGH));
  ASSERT_OK(blocker.WaitEntered());
  ASSERT_OK(xhci->DeviceOnline(queued_online_slot, 3, USB_SPEED_HIGH));

  auto queued_offline = ScheduleDeviceOffline(queued_offline_slot);
  xhci->RunUntilIdle();
  EXPECT_EQ(xhci->GetDeviceState(queued_offline_slot - 1u), nullptr);
  EXPECT_FALSE(queued_offline->done.load(std::memory_order_acquire));

  HubRemoveOp stop_remove(xhci, stop_hub.id, 1, blocker.release_completion());
  ASSERT_OK(stop_remove.WaitEnqueued());
  EXPECT_EQ(xhci->GetDeviceState(hub_child_slot - 1u), nullptr);
  EXPECT_FALSE(stop_remove.IsDone());

  // Release bus.AddDevice() once ShutdownDdkInteractionDispatcher() initiates dispatcher shutdown
  // so the queued callbacks are canceled by dispatcher shutdown rather than executing normally.
  std::thread release_on_stop([xhci, release = blocker.release_completion()] {
    WaitUntil([xhci] { return xhci->IsDdkInteractionShuttingDownForTesting(); });
    release->Signal();
  });
  auto join_release = fit::defer([release = blocker.release_completion(), &release_on_stop] {
    release->Signal();
    if (release_on_stop.joinable()) {
      release_on_stop.join();
    }
  });

  stopped_ = true;
  ASSERT_TRUE(driver_test().StopDriver().is_ok());
  release_on_stop.join();
  ASSERT_OK(stop_remove.WaitAndJoin());

  EXPECT_OK(stop_remove.status());
  EXPECT_EQ(bus_.devices().count(queued_online_slot - 1u), 0u);
  EXPECT_TRUE(queued_offline->done.load(std::memory_order_acquire));
  EXPECT_TRUE(queued_offline->is_error.load(std::memory_order_acquire));
  EXPECT_STATUS(queued_offline->status.load(std::memory_order_acquire), ZX_ERR_IO_NOT_PRESENT);
}

TEST_F(XhciMmioHarness, SetBusInterfaceNullFencesInFlightDdkCallback) {
  UsbXhci* xhci = driver_test().driver();
  auto doorbell = InstallCommandDoorbellListener();

  // (a) Off-dispatcher UsbHciSetBusInterface(nullptr) blocks until an in-flight bus.AddDevice()
  // callback completes, and prevents queued callbacks behind it from invoking bus.AddDevice().
  {
    const uint8_t in_flight_slot = AddressRootSlot(1);
    const uint8_t queued_slot = AddressRootSlot(2);
    AddDeviceBlocker blocker(&bus_, in_flight_slot - 1u, ZX_OK);
    ASSERT_OK(xhci->DeviceOnline(in_flight_slot, 1, USB_SPEED_HIGH));
    ASSERT_OK(blocker.WaitEntered());
    ASSERT_OK(xhci->DeviceOnline(queued_slot, 2, USB_SPEED_HIGH));

    libsync::Completion clear_done;
    std::thread clear_thread([xhci, &clear_done] {
      xhci->UsbHciSetBusInterface(nullptr);
      clear_done.Signal();
    });
    auto join_clear = fit::defer([&] {
      blocker.Release();
      if (clear_thread.joinable()) {
        clear_thread.join();
      }
    });

    ASSERT_TRUE(WaitUntil([xhci] { return !xhci->IsBusInterfaceValidForTesting(); }));
    EXPECT_STATUS(clear_done.Wait(zx::msec(20)), ZX_ERR_TIMED_OUT);

    blocker.Release();
    ASSERT_OK(clear_done.Wait(zx::sec(5)));
    clear_thread.join();
    EXPECT_EQ(bus_.devices().count(queued_slot - 1u), 0u);
  }

  // (b) Re-invoking UsbHciSetBusInterface(non_null) while bus_ is already valid clears bus_ and
  // fences in-flight DDK callbacks just like UsbHciSetBusInterface(nullptr).
  {
    xhci->UsbHciSetBusInterface(bus_.proto());
    ASSERT_TRUE(xhci->IsBusInterfaceValidForTesting());
    const uint8_t in_flight_slot = AddressRootSlot(1);
    const uint8_t queued_slot = AddressRootSlot(2);
    AddDeviceBlocker blocker(&bus_, in_flight_slot - 1u, ZX_OK);
    ASSERT_OK(xhci->DeviceOnline(in_flight_slot, 1, USB_SPEED_HIGH));
    ASSERT_OK(blocker.WaitEntered());
    ASSERT_OK(xhci->DeviceOnline(queued_slot, 2, USB_SPEED_HIGH));

    libsync::Completion clear_done;
    std::thread clear_thread([xhci, bus_proto = bus_.proto(), &clear_done] {
      xhci->UsbHciSetBusInterface(bus_proto);
      clear_done.Signal();
    });
    auto join_clear = fit::defer([&] {
      blocker.Release();
      if (clear_thread.joinable()) {
        clear_thread.join();
      }
    });

    ASSERT_TRUE(WaitUntil([xhci] { return !xhci->IsBusInterfaceValidForTesting(); }));
    EXPECT_STATUS(clear_done.Wait(zx::msec(20)), ZX_ERR_TIMED_OUT);

    blocker.Release();
    ASSERT_OK(clear_done.Wait(zx::sec(5)));
    clear_thread.join();
    EXPECT_FALSE(xhci->IsBusInterfaceValidForTesting());
    EXPECT_EQ(bus_.devices().count(queued_slot - 1u), 0u);
  }

  // (c) Reentrant UsbHciSetBusInterface(nullptr) from inside a bus.AddDevice() callback on
  // ddk_interaction_dispatcher_ clears bus_ without self-deadlocking.
  {
    xhci->UsbHciSetBusInterface(bus_.proto());
    const uint8_t reentrant_slot = AddressRootSlot(3);
    const uint8_t after_reentrant_slot = AddressRootSlot(4);
    auto reentrant_cleared = std::make_shared<libsync::Completion>();
    bus_.set_add_device_hook([xhci, reentrant_slot, reentrant_cleared](
                                 uint32_t device_id, uint32_t /*hub_id*/,
                                 usb_speed_t /*speed*/) -> std::optional<zx_status_t> {
      if (device_id == reentrant_slot - 1u) {
        xhci->UsbHciSetBusInterface(nullptr);
        reentrant_cleared->Signal();
      }
      return std::nullopt;
    });
    auto clear_hook = fit::defer([this] { bus_.clear_add_device_hook(); });

    ASSERT_OK(xhci->DeviceOnline(reentrant_slot, 3, USB_SPEED_HIGH));
    ASSERT_OK(reentrant_cleared->Wait(zx::sec(5)));
    ASSERT_OK(xhci->DeviceOnline(after_reentrant_slot, 4, USB_SPEED_HIGH));
    ASSERT_OK(xhci->FlushDdkCallbacksForTesting());
    EXPECT_EQ(bus_.devices().count(after_reentrant_slot - 1u), 0u);
  }

  // (d) Concurrent StopDriver() canceling the fence task still waits for ddk_interaction_shutdown_
  // before UsbHciSetBusInterface(nullptr) returns.
  {
    xhci->UsbHciSetBusInterface(bus_.proto());
    const uint8_t stop_slot = AddressRootSlot(1);
    AddDeviceBlocker blocker(&bus_, stop_slot - 1u, ZX_OK);
    ASSERT_OK(xhci->DeviceOnline(stop_slot, 1, USB_SPEED_HIGH));
    ASSERT_OK(blocker.WaitEntered());

    libsync::Completion clear_done;
    std::thread clear_thread([xhci, &clear_done] {
      xhci->UsbHciSetBusInterface(nullptr);
      clear_done.Signal();
    });
    std::thread release_on_stop([xhci, release = blocker.release_completion()] {
      WaitUntil([xhci] { return xhci->IsDdkInteractionShuttingDownForTesting(); });
      release->Signal();
    });
    auto cleanup = fit::defer([&] {
      blocker.Release();
      if (release_on_stop.joinable()) {
        release_on_stop.join();
      }
      if (clear_thread.joinable()) {
        clear_thread.join();
      }
    });

    ASSERT_TRUE(WaitUntil([xhci] { return !xhci->IsBusInterfaceValidForTesting(); }));
    EXPECT_STATUS(clear_done.Wait(zx::msec(20)), ZX_ERR_TIMED_OUT);

    stopped_ = true;
    ASSERT_TRUE(driver_test().StopDriver().is_ok());
    release_on_stop.join();
    ASSERT_OK(clear_done.Wait(zx::sec(5)));
    clear_thread.join();
  }

  // (e) Invoking UsbHciSetBusInterface(nullptr) after StopDriver() has already destroyed
  // ddk_interaction_dispatcher_ returns cleanly without blocking or crashing.
  xhci->UsbHciSetBusInterface(nullptr);
  EXPECT_FALSE(xhci->IsBusInterfaceValidForTesting());
}
}  // namespace

}  // namespace usb_xhci
