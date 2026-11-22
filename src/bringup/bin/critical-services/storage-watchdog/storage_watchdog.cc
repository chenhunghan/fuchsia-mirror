// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/storage_watchdog.h"

#include <fidl/fuchsia.kernel/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <zircon/syscalls/object.h>

#include <atomic>
#include <optional>
#include <print>
#include <string>
#include <thread>

#include "src/bringup/bin/critical-services/storage-watchdog/pager_fault.h"
#include "src/bringup/bin/critical-services/storage-watchdog/wedge_dump.h"
#include "src/lib/debug/backtrace-request.h"
#include "src/lib/fsl/handles/object_info.h"

namespace storage_watchdog {
namespace {

// Set once by Arm(), before |armed| is. Only read on the crashsvc thread, which is also the only
// thread that calls Trigger(), so it is never read after Trigger() hands it to the dump thread.
zx::job root_job;
std::atomic<bool> armed = false;
std::atomic<bool> triggered = false;

zx::job GetRootJob() {
  zx::result client = component::Connect<fuchsia_kernel::RootJob>();
  if (client.is_error()) {
    return {};
  }
  fidl::WireResult result = fidl::WireCall(*client)->Get();
  return result.ok() ? std::move(result.value().job) : zx::job();
}

// Does what fxfs's I/O watchdog does when an I/O request exceeds WATCHDOG_BACKTRACE_TIMEOUT.
void SimulateStall(zx::duration delay) {
  zx::nanosleep(zx::deadline_after(delay));
  fsl::SetCurrentThreadName("fxfs-watchdog:simulated-stall");
  std::println("storage-watchdog: simulating a storage stall");
  backtrace_request_all_threads();
}

// ZX_PROP_NAME is writable by any process with ZX_RIGHT_SET_PROPERTY on its own thread or process
// handle (including sandboxed processes), so verify that the caller is either critical-services
// itself (for SimulateStall) or a bootfs fxfs.cm holding a kernel pager object. fxfs maps some of
// the pager-backed VMOs it serves, so only its code has to come from bootfs.
bool IsTrustedCaller(const zx::process& process) {
  const zx_koid_t koid = fsl::GetKoid(process.get());
  if (koid != ZX_KOID_INVALID && koid == fsl::GetCurrentProcessKoid()) {
    return true;
  }
  if (fsl::GetObjectName(process.get()) != "fxfs.cm") {
    return false;
  }
  return IsBootfsPagerProcess(process);
}

}  // namespace

void Arm(uint64_t simulate_stall_secs) {
  root_job = GetRootJob();
  if (!root_job.is_valid()) {
    std::println("storage-watchdog: no root job, not arming");
    return;
  }
  armed = true;
  std::println("storage-watchdog: armed");
  if (simulate_stall_secs > 0) {
    const zx::duration delay =
        simulate_stall_secs <= static_cast<uint64_t>(zx::duration::infinite().to_secs())
            ? zx::sec(static_cast<int64_t>(simulate_stall_secs))
            : zx::duration::infinite();
    std::thread(SimulateStall, delay).detach();
  }
}

namespace {

// Starts the dump and reboot on another thread, once.
void Trigger(std::string reason, WedgeDumpFocus focus = {}) {
  if (!triggered.exchange(true)) {
    std::thread([reason = std::move(reason), job = std::move(root_job),
                 focus = std::move(focus)] mutable {
      DumpWedgeDiagnosticsAndReboot(reason, std::move(job), focus);
    }).detach();
  }
}

}  // namespace

bool OnBacktraceRequest(const zx::process& process, std::string_view thread_name) {
  if (!armed || !thread_name.starts_with(kTriggerThreadPrefix) || !IsTrustedCaller(process)) {
    return false;
  }
  Trigger("fxfs I/O watchdog fired (" + std::string(thread_name) + ")");
  return true;
}

bool OnPagerTimeout(const zx::process& process, const zx_exception_report_t& report) {
  if (!armed) {
    return false;
  }
  // Once triggered, |root_job| belongs to the dump thread. Keep holding later timed-out faults too:
  // handing them to crashsvc would kill the faulting process (e.g. archivist, whose death
  // terminates the root job) while the dump is still running.
  if (triggered) {
    return true;
  }
  // Only reads kernel object info, so this cannot block on the stuck pager. Resolve the backing
  // pager first so a fault inside fxfs.cm on a VMO served by driver_host records both processes.
  if (const std::optional<PagerOwner> pager =
          FindTrustedPagerFaultOwner(process, ExtractFaultingAddress(report), root_job)) {
    Trigger("a page fault in " + fsl::GetObjectName(process.get()) +
                " timed out waiting for the pager in " + pager->name,
            {
                .pager_process_name = pager->name,
                .pager_process_koid = pager->process_koid,
                .faulting_process_koid = fsl::GetKoid(process.get()),
            });
    return true;
  }
  if (IsTrustedCaller(process)) {
    Trigger("an fxfs page fault timed out waiting for the pager",
            {.faulting_process_koid = fsl::GetKoid(process.get())});
    return true;
  }
  return false;
}

}  // namespace storage_watchdog
