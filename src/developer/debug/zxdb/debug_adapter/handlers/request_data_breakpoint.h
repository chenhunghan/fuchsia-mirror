// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_DATA_BREAKPOINT_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_DATA_BREAKPOINT_H_

#include <lib/fit/function.h>

#include "src/developer/debug/zxdb/debug_adapter/context.h"

namespace zxdb {

void OnRequestDataBreakpointInfo(
    DebugAdapterContext* ctx, const dap::DataBreakpointInfoRequest& req,
    fit::function<void(dap::ResponseOrError<dap::DataBreakpointInfoResponse>)> callback);

dap::ResponseOrError<dap::SetDataBreakpointsResponse> OnRequestSetDataBreakpoints(
    DebugAdapterContext* ctx, const dap::SetDataBreakpointsRequest& req);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_DEBUG_ADAPTER_HANDLERS_REQUEST_DATA_BREAKPOINT_H_
