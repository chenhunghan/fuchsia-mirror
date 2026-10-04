// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_async_backtrace.h"

#include <lib/syslog/cpp/macros.h>

#include "src/developer/debug/zxdb/client/async_task_tree.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/source_file_provider_impl.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"

namespace dap {

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbAsyncBacktraceResponse, "", DAP_FIELD(tasks, "tasks"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbAsyncBacktraceRequest, "zxdb.AsyncBacktrace",
                              DAP_FIELD(threadId, "threadId"))

}  // namespace dap

namespace zxdb {

void OnRequestZxdbAsyncBacktrace(
    DebugAdapterContext* ctx, const dap::ZxdbAsyncBacktraceRequest& req,
    fit::function<void(dap::ResponseOrError<dap::ZxdbAsyncBacktraceResponse>)> callback) {
  if (req.threadId <= 0) {
    callback(dap::Error("Thread ID must be positive"));
    return;
  }

  Thread* thread = ctx->GetThread(static_cast<uint64_t>(req.threadId));
  if (!thread) {
    callback(dap::Error("Thread not found"));
    return;
  }

  const Process* process = thread->GetProcess();
  if (!process || !process->AllThreadsStopped()) {
    callback(dap::Error("All threads must be stopped before requesting async-backtrace."));
    return;
  }

  thread->GetAsyncTaskTree().Sync(
      thread->GetStack(), [weak_ctx = ctx->GetWeakPtr(), weak_thread = thread->GetWeakPtr(),
                           cb = std::move(callback)](const Err& sync_err, const Frame* /*frame*/) {
        if (!weak_ctx) {
          return;
        }

        if (!weak_thread) {
          cb(dap::Error("Thread not found"));
          return;
        }

        dap::ZxdbAsyncBacktraceResponse response = {};
        if (sync_err.has_error()) {
          FX_LOGS(DEBUG) << "Failed to sync async task tree: " << sync_err.msg();
          cb(response);
          return;
        }

        const Process* process = weak_thread->GetProcess();
        if (!process || !process->GetTarget() || !process->AllThreadsStopped()) {
          cb(response);
          return;
        }

        auto file_provider = SourceFileProviderImpl(process->GetTarget()->settings());
        response.tasks = FormatAsyncTaskTree(weak_thread->GetAsyncTaskTree(), file_provider);
        cb(response);
      });
}

}  // namespace zxdb
