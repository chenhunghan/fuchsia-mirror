// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_STORAGE_WATCHDOG_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_STORAGE_WATCHDOG_H_

#include <lib/zx/process.h>
#include <stdint.h>
#include <zircon/syscalls/exception.h>

#include <string_view>

namespace storage_watchdog {

// A debug watchdog for storage stalls.
//
// When one of fxfs's I/O requests has been blocked for 1 minute, fxfs's I/O watchdog makes a
// backtrace request from a thread named after it ("fxfs-watchdog:<op>-60s"). crashsvc, which
// handles every backtrace request in the system, passes the requesting process and thread name to
// OnBacktraceRequest(). If the watchdog is armed, DumpWedgeDiagnosticsAndReboot() then writes
// backtraces to the debuglog and reboots in a way that keeps the debuglog in the crashlog.
//
// fxfs's I/O watchdog runs on fxfs's executor, which stops when fxfs's own threads block on page
// faults that need the stuck storage. Then the first sign is the kernel giving up on one of those
// faults (kernel.userpager.overtime_timeout_seconds), which crashsvc sees as a fatal page fault
// with ZX_ERR_TIMED_OUT and passes to OnPagerTimeout().
//
// Such faults don't only happen in fxfs. When blobs are paged via the block driver rather than by
// fxfs, the pager of every package's code is pkg-cache or the storage driver host, so the first
// fault to time out can be in any process whose exceptions reach crashsvc (root-job hardware page
// faults; Starnix restricted-mode faults handled by starnix_kernel, child-job exception channels,
// and syscall copy faults returning ZX_ERR_TIMED_OUT do not reach crashsvc). OnPagerTimeout()
// therefore accepts any fault whose faulting VMO descends from a root pager VMO held via handle in
// a process that runs only bootfs code and holds a pager object (fxfs, pkg-cache, or the storage
// driver host; note that Zircon does not expose which zx::pager serves a VMO, so any root pager
// VMO held by that process, including via block_server AttachVmo, is attributed to it).
//
// critical-services only arms it if fuchsia.diagnostics.StorageStallWatchdogEnabled is set, which
// assembly does for non-user builds on boards that provide fuchsia::storage_stall_watchdog.

// The name prefix of the threads whose backtrace requests mean that storage is stuck.
inline constexpr std::string_view kTriggerThreadPrefix = "fxfs-watchdog:";

// Arms the watchdog. Gets the root job now, while the system is healthy.
//
// Test only: if |simulate_stall_secs| > 0 (from the optional
// fuchsia.diagnostics.StorageStallWatchdogSimulateStallSecs config capability), a thread named like
// fxfs's makes a backtrace request after |simulate_stall_secs| seconds. That reboots every boot, so
// unset it once done.
void Arm(uint64_t simulate_stall_secs = 0);

// Called for every backtrace request with the requesting process and thread name. If the watchdog
// is armed, the thread name starts with kTriggerThreadPrefix, and the process is fxfs (or
// critical-services itself during SimulateStall), starts the dump and reboot on another thread,
// once, and returns true; the caller should just resume the requesting thread.
bool OnBacktraceRequest(const zx::process& process, std::string_view thread_name);

// Called for every fatal page fault that failed with ZX_ERR_TIMED_OUT, i.e. the kernel stopped
// waiting for a pager. If the watchdog is armed and either the process is fxfs or the faulting VMO
// is served by a trusted bootfs storage pager (see above), starts the dump and reboot on another
// thread, once, and returns true (and keeps returning true for later faults while the dump runs);
// the caller should hold the exception until the reboot.
bool OnPagerTimeout(const zx::process& process, const zx_exception_report_t& report);

}  // namespace storage_watchdog

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_STORAGE_WATCHDOG_H_
