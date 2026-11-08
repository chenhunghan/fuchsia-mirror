// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CONSOLE_FORMAT_TRACEPOINT_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CONSOLE_FORMAT_TRACEPOINT_H_

#include "src/developer/debug/zxdb/format/output_buffer.h"

namespace zxdb {

class ConsoleContext;
class Tracepoint;
struct TracepointHitRecord;

OutputBuffer FormatTracepoint(const ConsoleContext* context, const Tracepoint* tracepoint);

OutputBuffer FormatTracepointList(ConsoleContext* context, int indent = 0);

OutputBuffer FormatTracepointHit(const ConsoleContext* context, const Tracepoint* tracepoint,
                                 const TracepointHitRecord& hit);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CONSOLE_FORMAT_TRACEPOINT_H_
