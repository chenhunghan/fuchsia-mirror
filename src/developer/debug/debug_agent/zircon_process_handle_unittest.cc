// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/debug_agent/zircon_process_handle.h"

#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/function.h>
#include <lib/zx/channel.h>
#include <lib/zx/process.h>
#include <zircon/syscalls/exception.h>
#include <zircon/syscalls/object.h>

#include <memory>
#include <utility>

#include <gtest/gtest.h>

#include "src/developer/debug/debug_agent/exception_handle.h"
#include "src/developer/debug/debug_agent/process_handle.h"
#include "src/developer/debug/debug_agent/process_handle_observer.h"
#include "src/developer/debug/debug_agent/test_utils.h"
#include "src/developer/debug/debug_agent/zircon_binary_launcher.h"
#include "src/developer/debug/shared/message_loop.h"
#include "src/developer/debug/shared/test_with_loop.h"

namespace debug_agent {
namespace {

class RecordingProcessHandleObserver : public ProcessHandleObserver {
 public:
  explicit RecordingProcessHandleObserver(fit::closure on_terminated = {})
      : on_terminated_(std::move(on_terminated)) {}

  bool terminated() const { return terminated_; }

  void OnProcessTerminated() override {
    terminated_ = true;
    if (on_terminated_) {
      on_terminated_();
    }
  }
  void OnThreadStarting(std::unique_ptr<ExceptionHandle> exception) override {}
  void OnThreadExiting(std::unique_ptr<ExceptionHandle> exception) override {}
  void OnException(std::unique_ptr<ExceptionHandle> exception) override {}
  void OnProcessStarting(std::unique_ptr<ProcessHandle> new_process_handle) override {}

 private:
  fit::closure on_terminated_;
  bool terminated_ = false;
};

struct ChildZirconProcess {
  std::unique_ptr<ZirconBinaryLauncher> launcher;
  zx::process process;
  std::unique_ptr<ZirconProcessHandle> handle;

  ~ChildZirconProcess() {
    handle.reset();
    if (process.is_valid()) {
      process.kill();
    }
  }
};

void CreateChildZirconProcess(ChildZirconProcess* out) {
  auto svc_dir = component::OpenServiceRoot();
  ASSERT_TRUE(svc_dir.is_ok()) << svc_dir.status_string();

  out->launcher = std::make_unique<ZirconBinaryLauncher>(*svc_dir);
  ASSERT_TRUE(
      out->launcher->Setup({"/pkg/test/debug_agent_unit_tests", "--gtest_filter=NonExistent"})
          .ok());

  std::unique_ptr<ProcessHandle> launcher_handle = out->launcher->GetProcess();
  ASSERT_TRUE(launcher_handle);

  const zx_status_t child_dup_status = launcher_handle->GetNativeHandle().duplicate(
      ZX_RIGHT_SAME_RIGHTS, &out->process);  // NOLINT(bugprone-signed-bitwise)
  ASSERT_ZX_EQ(child_dup_status, ZX_OK);
  zx::process handle_copy;
  const zx_status_t copy_dup_status = out->process.duplicate(
      ZX_RIGHT_SAME_RIGHTS, &handle_copy);  // NOLINT(bugprone-signed-bitwise)
  ASSERT_ZX_EQ(copy_dup_status, ZX_OK);
  out->handle = std::make_unique<ZirconProcessHandle>(std::move(handle_copy));
}

class ZirconProcessHandleTest : public debug::TestWithLoop {};

TEST_F(ZirconProcessHandleTest, AttachUpgradeAndDetachUpdatesIsAttached) {
  RecordingProcessHandleObserver observer;
  ChildZirconProcess child;
  ASSERT_NO_FATAL_FAILURE(CreateChildZirconProcess(&child));

  ProcessHandle::AttachConfig term_only_config;
  term_only_config.claim_exception_channel = false;
  ASSERT_TRUE(child.handle->Attach(&observer, term_only_config).ok());
  EXPECT_FALSE(child.handle->IsAttached());

  zx::channel test_ch;
  EXPECT_EQ(child.process.create_exception_channel(ZX_EXCEPTION_CHANNEL_DEBUGGER, &test_ch), ZX_OK);
  test_ch.reset();

  ProcessHandle::AttachConfig full_attach_config;
  full_attach_config.claim_exception_channel = true;
  ASSERT_TRUE(child.handle->Attach(&observer, full_attach_config).ok());
  EXPECT_TRUE(child.handle->IsAttached());
  EXPECT_EQ(child.process.create_exception_channel(ZX_EXCEPTION_CHANNEL_DEBUGGER, &test_ch),
            ZX_ERR_ALREADY_BOUND);

  child.handle->Detach();
  EXPECT_FALSE(child.handle->IsAttached());
  EXPECT_EQ(child.process.create_exception_channel(ZX_EXCEPTION_CHANNEL_DEBUGGER, &test_ch), ZX_OK);
}

TEST_F(ZirconProcessHandleTest, FailedUpgradePreservesTerminationWatch) {
  RecordingProcessHandleObserver observer([&loop = loop()] { loop.QuitNow(); });
  ChildZirconProcess child;
  ASSERT_NO_FATAL_FAILURE(CreateChildZirconProcess(&child));

  ProcessHandle::AttachConfig term_only_config;
  term_only_config.claim_exception_channel = false;
  ASSERT_TRUE(child.handle->Attach(&observer, term_only_config).ok());
  EXPECT_FALSE(child.handle->IsAttached());

  zx::channel external_debugger_channel;
  ASSERT_ZX_EQ(child.process.create_exception_channel(ZX_EXCEPTION_CHANNEL_DEBUGGER,
                                                      &external_debugger_channel),
               ZX_OK);

  ProcessHandle::AttachConfig full_attach_config;
  full_attach_config.claim_exception_channel = true;
  debug::Status upgrade_status = child.handle->Attach(&observer, full_attach_config);
  EXPECT_TRUE(upgrade_status.has_error());
  EXPECT_FALSE(child.handle->IsAttached());

  auto timed_out = std::make_shared<bool>(false);
  loop().PostTimer(FROM_HERE, 10000, [timed_out, &loop = loop()] {
    *timed_out = true;
    loop.QuitNow();
  });
  ASSERT_ZX_EQ(child.process.kill(), ZX_OK);
  loop().Run();

  EXPECT_FALSE(*timed_out);
  EXPECT_TRUE(observer.terminated());
}

TEST_F(ZirconProcessHandleTest, TerminationOnlyDetachPreservesBreakOnLoad) {
  RecordingProcessHandleObserver observer;
  ChildZirconProcess child;
  ASSERT_NO_FATAL_FAILURE(CreateChildZirconProcess(&child));

  constexpr uintptr_t kExpectedBreakOnLoad = 0x123456;
  ASSERT_ZX_EQ(child.process.set_property(ZX_PROP_PROCESS_BREAK_ON_LOAD, &kExpectedBreakOnLoad,
                                          sizeof(kExpectedBreakOnLoad)),
               ZX_OK);

  ProcessHandle::AttachConfig term_only_config;
  term_only_config.claim_exception_channel = false;
  ASSERT_TRUE(child.handle->Attach(&observer, term_only_config).ok());
  EXPECT_FALSE(child.handle->IsAttached());

  child.handle->Detach();
  EXPECT_FALSE(child.handle->IsAttached());

  uintptr_t break_on_load = 0;
  ASSERT_ZX_EQ(child.process.get_property(ZX_PROP_PROCESS_BREAK_ON_LOAD, &break_on_load,
                                          sizeof(break_on_load)),
               ZX_OK);
  EXPECT_EQ(break_on_load, kExpectedBreakOnLoad);
}

}  // namespace
}  // namespace debug_agent
