// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_ASYNC_BACKTRACE_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_ASYNC_BACKTRACE_H_

#include <lib/fit/function.h>

#include <dap/protocol.h>
#include <dap/typeof.h>
#include <dap/types.h>

#include "src/developer/debug/zxdb/debug_adapter/async_backtrace_subscription.h"

namespace dap {

struct ZxdbAsyncBacktraceResponse : public Response {
  array<AsyncTaskNode> tasks;
};

DAP_DECLARE_STRUCT_TYPEINFO(ZxdbAsyncBacktraceResponse);

struct ZxdbAsyncBacktraceRequest : public Request {
  using Response = ZxdbAsyncBacktraceResponse;
  integer threadId = 0;
};

DAP_DECLARE_STRUCT_TYPEINFO(ZxdbAsyncBacktraceRequest);

}  // namespace dap

namespace zxdb {

class DebugAdapterContext;

void OnRequestZxdbAsyncBacktrace(
    DebugAdapterContext* ctx, const dap::ZxdbAsyncBacktraceRequest& req,
    fit::function<void(dap::ResponseOrError<dap::ZxdbAsyncBacktraceResponse>)> callback);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_ASYNC_BACKTRACE_H_
