// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/developer/debug/debug_agent/debugged_process.h"

#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <zircon/syscalls/object.h>

#include <memory>

#include <gtest/gtest.h>

#include "src/developer/debug/debug_agent/arch.h"
#include "src/developer/debug/debug_agent/breakpoint.h"
#include "src/developer/debug/debug_agent/debug_agent.h"
#include "src/developer/debug/debug_agent/debugged_thread.h"
#include "src/developer/debug/debug_agent/hardware_breakpoint.h"
#include "src/developer/debug/debug_agent/mock_debug_agent_harness.h"
#include "src/developer/debug/debug_agent/mock_exception_handle.h"
#include "src/developer/debug/debug_agent/mock_process.h"
#include "src/developer/debug/debug_agent/mock_thread.h"
#include "src/developer/debug/debug_agent/mock_thread_handle.h"
#include "src/developer/debug/debug_agent/software_breakpoint.h"
#include "src/developer/debug/debug_agent/test_utils.h"
#include "src/developer/debug/debug_agent/zircon_binary_launcher.h"
#include "src/developer/debug/ipc/protocol.h"
#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/shared/test_with_loop.h"

namespace debug_agent {
namespace {

class MockProcessDelegate : public Breakpoint::ProcessDelegate {
 public:
  debug::Status RegisterBreakpoint(Breakpoint*, zx_koid_t, uint64_t) override {
    return debug::Status();
  }
  void UnregisterBreakpoint(Breakpoint* bp, zx_koid_t process_koid, uint64_t address) override {}

  debug::Status RegisterWatchpoint(Breakpoint*, zx_koid_t, const debug::AddressRange&) override {
    return debug::Status();
  }
  void UnregisterWatchpoint(Breakpoint*, zx_koid_t, const debug::AddressRange&) override {}
};

// Fills a vector with the breakpoint instruction for the current architecture. The size will vary
// by architecture.
std::vector<uint8_t> GetBreakpointMemory() {
  std::vector<uint8_t> result;
  result.resize(arch::kBreakInstructionSize);
  memcpy(result.data(), &arch::kBreakInstruction, arch::kBreakInstructionSize);
  return result;
}

// If |thread| is null, it means a process-wide breakpoint.
debug_ipc::ProcessBreakpointSettings CreateLocation(zx_koid_t process_koid, zx_koid_t thread_koid,
                                                    uint64_t address) {
  debug_ipc::ProcessBreakpointSettings location = {};
  location.id = {.process = process_koid, .thread = thread_koid};
  location.address = address;

  return location;
}

debug_ipc::ProcessBreakpointSettings CreateLocation(zx_koid_t process_koid, zx_koid_t thread_koid,
                                                    const debug::AddressRange& range) {
  debug_ipc::ProcessBreakpointSettings location = {};
  location.id = {.process = process_koid, .thread = thread_koid};
  location.address_range = range;

  return location;
}

debug::AddressRange SetLocation(Breakpoint* breakpoint, zx_koid_t koid,
                                const debug::AddressRange& range) {
  debug_ipc::BreakpointSettings settings;
  settings.type = debug_ipc::BreakpointType::kWrite;
  settings.locations.push_back(CreateLocation(koid, 0, range));
  breakpoint->SetSettings(settings);

  return range;
}

// Tests -------------------------------------------------------------------------------------------

constexpr zx_koid_t kProcessKoid = 0x1;
const std::string kProcessName = "process-name";
constexpr uint64_t kAddress1 = 0x1234;
constexpr uint64_t kAddress2 = 0x5678;
constexpr uint64_t kAddress3 = 0x9abc;
constexpr uint64_t kAddress4 = 0xdef0;

constexpr debug::AddressRange kAddressRange1 = {0x1, 0x2};

TEST(DebuggedProcess, RegisterBreakpoints) {
  MockProcessDelegate process_delegate;
  MockProcess process(nullptr, kProcessKoid, kProcessName);

  // There needs to be memory backing the breakpoints so set some memory here. When the breakpoint
  // is uninstalled, the breakpoint expects that it will be the debug break instruction. So we
  // handle both cases and always report a breakpoint at these addresses.
  process.mock_process_handle().mock_memory().AddMemory(kAddress1, GetBreakpointMemory());
  process.mock_process_handle().mock_memory().AddMemory(kAddress2, GetBreakpointMemory());
  process.mock_process_handle().mock_memory().AddMemory(kAddress3, GetBreakpointMemory());
  process.mock_process_handle().mock_memory().AddMemory(kAddress4, GetBreakpointMemory());

  debug_ipc::BreakpointSettings settings;
  settings.type = debug_ipc::BreakpointType::kSoftware;
  settings.locations.push_back(CreateLocation(kProcessKoid, 0, kAddress1));
  settings.locations.push_back(CreateLocation(kProcessKoid, 0, kAddress2));
  settings.locations.push_back(CreateLocation(kProcessKoid, 0, kAddress3));
  Breakpoint breakpoint(&process_delegate);
  breakpoint.SetSettings(settings);

  ASSERT_TRUE(process.RegisterBreakpoint(&breakpoint, kAddress1).ok());

  ASSERT_EQ(process.software_breakpoints().size(), 1u);
  auto it = process.software_breakpoints().begin();
  EXPECT_EQ(it++->first, kAddress1);
  EXPECT_EQ(it, process.software_breakpoints().end());

  // Add 2 other breakpoints.
  ASSERT_TRUE(process.RegisterBreakpoint(&breakpoint, kAddress2).ok());
  ASSERT_TRUE(process.RegisterBreakpoint(&breakpoint, kAddress3).ok());
  ASSERT_EQ(process.software_breakpoints().size(), 3u);
  it = process.software_breakpoints().begin();
  EXPECT_EQ(it++->first, kAddress1);
  EXPECT_EQ(it++->first, kAddress2);
  EXPECT_EQ(it++->first, kAddress3);
  EXPECT_EQ(it, process.software_breakpoints().end());

  // Unregister a breakpoint.
  process.UnregisterBreakpoint(&breakpoint, kAddress1);
  ASSERT_EQ(process.software_breakpoints().size(), 2u);
  it = process.software_breakpoints().begin();
  EXPECT_EQ(it++->first, kAddress2);
  EXPECT_EQ(it++->first, kAddress3);
  EXPECT_EQ(it, process.software_breakpoints().end());

  // Register a hardware breakpoint.
  Breakpoint hw_breakpoint(&process_delegate);
  debug_ipc::BreakpointSettings hw_settings;
  hw_settings.type = debug_ipc::BreakpointType::kHardware;
  hw_settings.locations.push_back(CreateLocation(kProcessKoid, 0, kAddress4));
  hw_breakpoint.SetSettings(hw_settings);

  ASSERT_TRUE(process.RegisterBreakpoint(&hw_breakpoint, kAddress3).ok());
  ASSERT_TRUE(process.RegisterBreakpoint(&hw_breakpoint, kAddress4).ok());

  // Should've inserted 2 HW breakpoint.
  ASSERT_EQ(process.software_breakpoints().size(), 2u);
  ASSERT_EQ(process.hardware_breakpoints().size(), 2u);
  auto hw_it = process.hardware_breakpoints().begin();
  EXPECT_EQ(hw_it++->first, kAddress3);
  EXPECT_EQ(hw_it++->first, kAddress4);
  EXPECT_EQ(hw_it, process.hardware_breakpoints().end());

  // Remove a hardware breakpoint.
  process.UnregisterBreakpoint(&hw_breakpoint, kAddress3);
  ASSERT_EQ(process.software_breakpoints().size(), 2u);
  ASSERT_EQ(process.hardware_breakpoints().size(), 1u);
  hw_it = process.hardware_breakpoints().begin();
  EXPECT_EQ(hw_it++->first, kAddress4);
  EXPECT_EQ(hw_it, process.hardware_breakpoints().end());

  // Add a watchpoint.
  Breakpoint wp_breakpoint(&process_delegate);
  debug_ipc::BreakpointSettings wp_settings;
  wp_settings.type = debug_ipc::BreakpointType::kWrite;
  wp_settings.locations.push_back(CreateLocation(kProcessKoid, 0, kAddressRange1));
  wp_breakpoint.SetSettings(wp_settings);

  ASSERT_TRUE(process.RegisterWatchpoint(&wp_breakpoint, kAddressRange1).ok());
  ASSERT_EQ(process.software_breakpoints().size(), 2u);
  ASSERT_EQ(process.hardware_breakpoints().size(), 1u);
  ASSERT_EQ(process.watchpoints().size(), 1u);
  auto wp_it = process.watchpoints().begin();
  EXPECT_EQ(wp_it++->first, kAddressRange1);
  EXPECT_EQ(wp_it, process.watchpoints().end());
}

TEST(DebuggedProcess, WatchpointRegistration) {
  MockProcessDelegate process_delegate;
  Breakpoint breakpoint(&process_delegate);

  MockProcess process(nullptr, kProcessKoid, kProcessName);

  // 1 byte.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 1};
    SetLocation(&breakpoint, process.koid(), range);
    ASSERT_TRUE(process.RegisterWatchpoint(&breakpoint, range).ok());
  }

  // 2 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 2};
    SetLocation(&breakpoint, process.koid(), range);

    // Only aligned values should work.
    bool expected_ok = (range.begin() & 0b1u) == 0;
    SCOPED_TRACE(range.ToString());
    ASSERT_EQ(process.RegisterWatchpoint(&breakpoint, range).ok(), expected_ok);
  }

  // 3 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 3};
    SetLocation(&breakpoint, process.koid(), range);
    ASSERT_TRUE(process.RegisterWatchpoint(&breakpoint, range).has_error());
  }

  // 4 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 4};
    SetLocation(&breakpoint, process.koid(), range);

    // Only aligned values should work.
    bool expected_ok = (range.begin() & 0b11u) == 0;
    SCOPED_TRACE(range.ToString());
    ASSERT_EQ(process.RegisterWatchpoint(&breakpoint, range).ok(), expected_ok);
  }

  // 5 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 5};
    SetLocation(&breakpoint, process.koid(), range);
    ASSERT_TRUE(process.RegisterWatchpoint(&breakpoint, range).has_error());
  }

  // 6 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 6};
    SetLocation(&breakpoint, process.koid(), range);
    ASSERT_TRUE(process.RegisterWatchpoint(&breakpoint, range).has_error());
  }

  // 6 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 6};
    SetLocation(&breakpoint, process.koid(), range);
    ASSERT_TRUE(process.RegisterWatchpoint(&breakpoint, range).has_error());
  }

  // 7 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 7};
    SetLocation(&breakpoint, process.koid(), range);
    ASSERT_TRUE(process.RegisterWatchpoint(&breakpoint, range).has_error());
  }

  // 8 bytes.
  for (uint32_t i = 0; i < 16; i++) {
    debug::AddressRange range = {0x10 + i, 0x10 + i + 8};
    SetLocation(&breakpoint, process.koid(), range);

    // Only aligned values should work.
    bool expected_ok = (range.begin() & 0b111u) == 0;
    SCOPED_TRACE(range.ToString());
    ASSERT_EQ(process.RegisterWatchpoint(&breakpoint, range).ok(), expected_ok);
  }
}

TEST(DebuggedProcess, DetachFromProcess) {
  MockProcess process(nullptr, kProcessKoid, kProcessName);
  MockThread* thread = process.AddThread(2);

  ASSERT_TRUE(thread->running());
  process.ClientSuspendAllThreads();
  ASSERT_FALSE(thread->running());
  process.DetachFromProcess();
  ASSERT_TRUE(thread->running());
}

TEST(DebuggedProcess, DetachFromProcessWithBreakpoints) {
  MockProcess process(nullptr, kProcessKoid, kProcessName);
  MockThread* thread = process.AddThread(1234);

  MockProcessDelegate delegate;

  Breakpoint bp1 = Breakpoint(&delegate);
  debug_ipc::BreakpointSettings settings;
  settings.type = debug_ipc::BreakpointType::kHardware;
  settings.locations.push_back(CreateLocation(process.koid(), thread->koid(), kAddress1));
  bp1.SetSettings(settings);

  ASSERT_TRUE(process.RegisterBreakpoint(&bp1, kAddress1).ok());

  Breakpoint bp2 = Breakpoint(&delegate);
  debug_ipc::BreakpointSettings settings2;
  settings2.type = debug_ipc::BreakpointType::kSoftware;
  settings2.locations.push_back(CreateLocation(process.koid(), thread->koid(), kAddress2));
  bp2.SetSettings(settings2);

  // The software breakpoint needs some memory backing it.
  process.mock_process_handle().mock_memory().AddMemory(kAddress2, GetBreakpointMemory());

  ASSERT_TRUE(process.RegisterBreakpoint(&bp2, kAddress2).ok());

  process.DetachFromProcess();

  // Now the thread should be running again and there shouldn't be any more breakpoints.
  EXPECT_TRUE(process.hardware_breakpoints().empty());
  EXPECT_TRUE(process.software_breakpoints().empty());
  EXPECT_TRUE(thread->running());
}

TEST(DebuggedProcess, WeakAttachSkipsLoaderBreakpoint) {
  MockDebugAgentHarness harness;
  DebuggedProcessCreateInfo info(std::make_unique<MockProcessHandle>(kProcessKoid, kProcessName));
  info.priority = debug_ipc::AttachConfig::Priority::kWeak;
  auto unique_process = std::make_unique<MockProcess>(harness.debug_agent(), std::move(info));
  DebuggedProcess* process = unique_process.get();

  harness.debug_agent()->InjectProcessForTest(std::move(unique_process));

  process->HandleLoaderBreakpoint(MockProcessHandle::kLoaderBreakpointAddress);

  // Should not have sent modules.
  EXPECT_TRUE(harness.stream_backend()->modules().empty());
}

TEST(DebuggedProcess, ExceptionEnsuresThreadsAreSynced) {
  MockDebugAgentHarness harness;
  auto mock_process = harness.AddProcess(kProcessKoid);

  // There should be no threads initially.
  ASSERT_TRUE(mock_process->GetThreads().empty());

  constexpr zx_koid_t kThreadKoid = 2;

  // Inject a thread to the underlying process handle object, which will be the thread that receives
  // the exception.
  mock_process->mock_process_handle().set_threads({MockThreadHandle(kThreadKoid)});

  // Dispatch the exception.
  mock_process->InjectException(std::make_unique<MockExceptionHandle>(kProcessKoid, kThreadKoid));

  // Now the DebuggedProcess should see it's child thread even though it didn't get a ThreadStarting
  // notification.
  ASSERT_EQ(mock_process->GetThreads().size(), 1u);
  EXPECT_EQ(mock_process->GetThreads()[0]->koid(), kThreadKoid);

  // We should also receive an exception notification.
  ASSERT_EQ(harness.stream_backend()->exceptions().size(), 1u);
  EXPECT_EQ(harness.stream_backend()->exceptions()[0].thread.id.process, kProcessKoid);
  EXPECT_EQ(harness.stream_backend()->exceptions()[0].thread.id.thread, kThreadKoid);
}

TEST(DebuggedProcessTest, ClientResumeUnsetsSuspendNewThreads) {
  MockDebugAgentHarness harness;

  constexpr uint64_t kThread1Koid = 1234;
  constexpr uint64_t kThread2Koid = 1235;
  constexpr uint64_t kThread3Koid = 1236;

  auto* mock_process = harness.AddProcess(kProcessKoid);
  mock_process->AddThread(kThread1Koid);

  ASSERT_TRUE(mock_process);
  ASSERT_EQ(mock_process->GetThreads().size(), 1u);

  std::ignore = mock_process->ClientSuspendAllThreads();

  auto* debugged_thread = mock_process->GetThread(kThread1Koid);
  ASSERT_TRUE(debugged_thread);
  EXPECT_TRUE(debugged_thread->is_client_suspended());

  // Inject the thread starting notification. The thread should now be suspended. We have to inject
  // the new thread into the MockProcessHandle so DebuggedProcess can re-populate its threads to
  // find the new one.
  MockThreadHandle thread2_handle(kThread2Koid);
  mock_process->mock_process_handle().AddThreadAndSendEvent(std::move(thread2_handle));

  auto* debugged_thread2 = mock_process->GetThread(kThread2Koid);
  ASSERT_TRUE(debugged_thread2);
  EXPECT_TRUE(debugged_thread2->is_client_suspended());

  // Issue the resume as separate ProcessThreadIds, which simulates how zxdb does a "Process
  // Continue".
  debug_ipc::ResumeRequest resume_request;
  resume_request.how = debug_ipc::ResumeRequest::How::kResolveAndContinue;
  resume_request.ids = {
      {.process = kProcessKoid, .thread = kThread1Koid},
      {.process = kProcessKoid, .thread = kThread2Koid},
  };
  mock_process->OnResume(resume_request);

  EXPECT_FALSE(debugged_thread->is_client_suspended());
  EXPECT_FALSE(debugged_thread2->is_client_suspended());

  MockThreadHandle thread3_handle(kThread3Koid);
  mock_process->mock_process_handle().AddThreadAndSendEvent(std::move(thread3_handle));
  auto* thread3 = mock_process->GetThread(kThread3Koid);
  ASSERT_TRUE(thread3);
  EXPECT_FALSE(thread3->is_client_suspended());
}

class ThreadStartingExceptionHandle : public MockExceptionHandle {
 public:
  ThreadStartingExceptionHandle(uint64_t process_koid, const MockThreadHandle* thread_handle,
                                bool* exception_open_out, bool* was_suspended_at_close_out)
      : MockExceptionHandle(process_koid,
                            thread_handle ? thread_handle->GetKoid() : ZX_KOID_INVALID),
        thread_handle_(thread_handle),
        exception_open_out_(exception_open_out),
        was_suspended_at_close_out_(was_suspended_at_close_out) {
    FX_CHECK(thread_handle_);
    if (exception_open_out_) {
      *exception_open_out_ = true;
    }
  }

  ~ThreadStartingExceptionHandle() override {
    if (was_suspended_at_close_out_) {
      *was_suspended_at_close_out_ = thread_handle_->is_suspended();
    }
    if (exception_open_out_) {
      *exception_open_out_ = false;
    }
  }

  std::unique_ptr<ThreadHandle> GetThreadHandle() const override {
    return std::make_unique<MockThreadHandle>(*thread_handle_);
  }

 private:
  const MockThreadHandle* thread_handle_ = nullptr;
  bool* exception_open_out_ = nullptr;
  bool* was_suspended_at_close_out_ = nullptr;
};

TEST(DebuggedProcessTest, OnThreadStartingSuspendsBeforeReleasingStartException) {
  bool exception_open = false;
  bool was_suspended_at_close = false;
  bool notified = false;
  constexpr zx_koid_t kThreadKoid = 43;
  MockThreadHandle thread_handle(kThreadKoid, "paused-worker");

  MockDebugAgentHarness harness;
  auto clear_callback =
      fit::defer([&harness] { harness.stream_backend()->set_notify_thread_starting_callback({}); });
  auto* mock_process = harness.AddProcess(kProcessKoid);
  mock_process->ClientSuspendAllThreads();

  harness.stream_backend()->set_notify_thread_starting_callback(
      [&](const debug_ipc::NotifyThreadStarting& notify) {
        notified = true;
        EXPECT_FALSE(exception_open);
        EXPECT_NE(mock_process->GetThread(kThreadKoid), nullptr);
        EXPECT_EQ(kProcessKoid, notify.record.id.process);
        EXPECT_EQ(kThreadKoid, notify.record.id.thread);
        EXPECT_EQ("paused-worker", notify.record.name);
      });

  static_cast<ProcessHandleObserver*>(mock_process)
      ->OnThreadStarting(std::make_unique<ThreadStartingExceptionHandle>(
          kProcessKoid, &thread_handle, &exception_open, &was_suspended_at_close));

  EXPECT_TRUE(notified);
  EXPECT_TRUE(was_suspended_at_close);
  EXPECT_FALSE(exception_open);
  EXPECT_NE(mock_process->GetThread(kThreadKoid), nullptr);
  ASSERT_EQ(1u, harness.stream_backend()->thread_starts().size());
}

TEST(DebuggedProcessTest, OnThreadStartingIgnoresDuplicateThreadKoid) {
  MockDebugAgentHarness harness;
  auto* mock_process = harness.AddProcess(kProcessKoid);

  constexpr zx_koid_t kThreadKoid = 44;
  MockThreadHandle thread_handle(kThreadKoid, "worker");
  mock_process->mock_process_handle().set_threads({thread_handle});
  mock_process->PopulateCurrentThreads();

  ASSERT_NE(mock_process->GetThread(kThreadKoid), nullptr);
  ASSERT_EQ(1u, harness.stream_backend()->thread_starts().size());

  static_cast<ProcessHandleObserver*>(mock_process)
      ->OnThreadStarting(std::make_unique<MockExceptionHandle>(kProcessKoid, kThreadKoid));

  EXPECT_EQ(1u, harness.stream_backend()->thread_starts().size());
  EXPECT_EQ(1u, mock_process->GetThreads().size());
}

class DebuggedProcessZirconTest : public debug::TestWithLoop {
 protected:
  // Launches a zircon child process and runs the loop until its initial thread-start arrives.
  void RunThreadStartOnZirconProcess(bool suspend_before_start,
                                     debug_ipc::NotifyThreadStarting* out_notify,
                                     zx_thread_state_t* out_thread_state_at_notify) {
    auto svc_dir = component::OpenServiceRoot();
    ASSERT_TRUE(svc_dir.is_ok()) << svc_dir.status_string();

    ZirconBinaryLauncher launcher(*svc_dir);
    ASSERT_TRUE(
        launcher.Setup({"/pkg/test/debug_agent_unit_tests", "--gtest_filter=NonExistent"}).ok());

    bool got_thread_start = false;
    auto timed_out = std::make_shared<bool>(false);
    DebuggedProcess* process = nullptr;
    MockDebugAgentHarness harness;
    auto clear_callback = fit::defer(
        [&harness] { harness.stream_backend()->set_notify_thread_starting_callback({}); });

    std::unique_ptr<ProcessHandle> process_handle = launcher.GetProcess();
    ASSERT_TRUE(process_handle);

    zx::process child_process;
    const zx_status_t dup_status = process_handle->GetNativeHandle().duplicate(
        ZX_RIGHT_SAME_RIGHTS, &child_process);  // NOLINT(bugprone-signed-bitwise)
    ASSERT_ZX_EQ(dup_status, ZX_OK);
    auto kill_child = fit::defer([&child_process] {
      if (child_process.is_valid()) {
        child_process.kill();
      }
    });

    auto debugged_process = std::make_unique<DebuggedProcess>(harness.debug_agent());
    process = debugged_process.get();
    ASSERT_TRUE(debugged_process->Init(DebuggedProcessCreateInfo(std::move(process_handle))).ok());
    harness.debug_agent()->InjectProcessForTest(std::move(debugged_process));

    if (suspend_before_start) {
      process->ClientSuspendAllThreads();
    }

    harness.stream_backend()->set_notify_thread_starting_callback(
        [&](const debug_ipc::NotifyThreadStarting& notify) {
          if (!got_thread_start) {
            got_thread_start = true;
            *out_notify = notify;
            DebuggedThread* thread = process->GetThread(notify.record.id.thread);
            EXPECT_NE(nullptr, thread);
            if (thread && out_thread_state_at_notify) {
              zx_info_thread_t thread_info = {};
              EXPECT_ZX_EQ(thread->thread_handle().GetNativeHandle().get_info(
                               ZX_INFO_THREAD, &thread_info, sizeof(thread_info), nullptr, nullptr),
                           ZX_OK);
              *out_thread_state_at_notify = thread_info.state;
            }
            loop().QuitNow();
          }
        });
    loop().PostTimer(FROM_HERE, 10000, [timed_out, loop = &loop()] {
      *timed_out = true;
      loop->QuitNow();
    });

    ASSERT_TRUE(launcher.Start().ok());
    loop().Run();

    ASSERT_FALSE(*timed_out);
    ASSERT_TRUE(got_thread_start);
    ASSERT_EQ(1u, harness.stream_backend()->thread_starts().size());
  }
};

TEST_F(DebuggedProcessZirconTest, OnThreadStartingEmitsRunningWhenZirconProcessNotSuspended) {
  debug_ipc::NotifyThreadStarting notify;
  zx_thread_state_t thread_state = ZX_THREAD_STATE_NEW;
  ASSERT_NO_FATAL_FAILURE(
      RunThreadStartOnZirconProcess(/*suspend_before_start=*/false, &notify, &thread_state));

  EXPECT_EQ(ZX_THREAD_STATE_BLOCKED_EXCEPTION, thread_state);
  EXPECT_EQ(debug_ipc::ThreadRecord::State::kRunning, notify.record.state);
  EXPECT_EQ(debug_ipc::ThreadRecord::BlockedReason::kNotBlocked, notify.record.blocked_reason);
  EXPECT_EQ(debug_ipc::ThreadRecord::StackAmount::kNone, notify.record.stack_amount);
  EXPECT_TRUE(notify.record.frames.empty());
}

TEST_F(DebuggedProcessZirconTest, OnThreadStartingEmitsSuspendedWhenZirconProcessPaused) {
  debug_ipc::NotifyThreadStarting notify;
  zx_thread_state_t thread_state = ZX_THREAD_STATE_NEW;
  ASSERT_NO_FATAL_FAILURE(
      RunThreadStartOnZirconProcess(/*suspend_before_start=*/true, &notify, &thread_state));

  EXPECT_EQ(ZX_THREAD_STATE_SUSPENDED, thread_state);
  EXPECT_EQ(debug_ipc::ThreadRecord::State::kSuspended, notify.record.state);
  EXPECT_EQ(debug_ipc::ThreadRecord::BlockedReason::kNotBlocked, notify.record.blocked_reason);
  EXPECT_EQ(debug_ipc::ThreadRecord::StackAmount::kMinimal, notify.record.stack_amount);
  EXPECT_FALSE(notify.record.frames.empty());
}

}  // namespace
}  // namespace debug_agent
