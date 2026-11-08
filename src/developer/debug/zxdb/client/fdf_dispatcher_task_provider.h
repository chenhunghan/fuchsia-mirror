// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_FDF_DISPATCHER_TASK_PROVIDER_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_FDF_DISPATCHER_TASK_PROVIDER_H_

#include <memory>
#include <vector>

#include "src/developer/debug/zxdb/client/async_task_provider.h"

namespace zxdb {

class Err;
class Frame;

// Implements AsyncTaskProvider for Fuchsia Driver Framework fdf::Dispatcher
// asynchronous tasks and callbacks.
//
// Supports inspecting active tasks, queued tasks, delayed tasks, and registered
// callbacks on fdf::Dispatcher threads.
class FdfDispatcherTaskProvider : public AsyncTaskProvider {
 public:
  FdfDispatcherTaskProvider();
  ~FdfDispatcherTaskProvider() override;

  // AsyncTaskProvider implementation:
  bool CanHandle(Frame* frame) const override;
  void GetTasks(
      Frame* frame,
      fit::callback<void(const Err&, std::vector<std::unique_ptr<AsyncTask>>)> cb) override;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_FDF_DISPATCHER_TASK_PROVIDER_H_
