// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_DEBUG_AGENT_MOCK_STREAM_BACKEND_H_
#define SRC_DEVELOPER_DEBUG_DEBUG_AGENT_MOCK_STREAM_BACKEND_H_

#include <lib/fit/function.h>

#include <utility>

#include "src/developer/debug/debug_agent/local_stream_backend.h"
#include "src/developer/debug/ipc/protocol.h"

namespace debug_agent {

class MockStreamBackend : public LocalStreamBackend {
 public:
  void HandleAttach(debug_ipc::AttachReply attach_reply) override {
    attach_replies_.push_back(std::move(attach_reply));
  }

  void HandleNotifyComponentStarting(debug_ipc::NotifyComponentStarting notification) override {
    component_starts_.push_back(std::move(notification));
  }

  void HandleNotifyProcessStarting(debug_ipc::NotifyProcessStarting notification) override {
    process_starts_.push_back(std::move(notification));
  }

  void HandleNotifyModules(debug_ipc::NotifyModules modules) override {
    modules_.push_back(std::move(modules));
  }

  const std::vector<debug_ipc::AttachReply>& attach_replies() const { return attach_replies_; }

  const std::vector<debug_ipc::NotifyComponentStarting>& component_starts() const {
    return component_starts_;
  }

  const std::vector<debug_ipc::NotifyProcessStarting>& process_starts() const {
    return process_starts_;
  }
  const std::vector<debug_ipc::NotifyModules>& modules() const { return modules_; }

  void HandleNotifyException(debug_ipc::NotifyException exception) override {
    exceptions_.push_back(std::move(exception));
  }

  const std::vector<debug_ipc::NotifyException>& exceptions() const { return exceptions_; }

  void HandleNotifyFilterCreated(debug_ipc::NotifyFilterCreated filter) override {
    filters_.push_back(filter);
  }

  const std::vector<debug_ipc::NotifyFilterCreated>& filters() const { return filters_; }

  void set_notify_thread_starting_callback(
      fit::function<void(const debug_ipc::NotifyThreadStarting&)> cb) {
    notify_thread_starting_callback_ = std::move(cb);
  }

  void HandleNotifyThreadStarting(debug_ipc::NotifyThreadStarting notification) override {
    if (notify_thread_starting_callback_) {
      notify_thread_starting_callback_(notification);
    }
    thread_starts_.push_back(std::move(notification));
  }

  const std::vector<debug_ipc::NotifyThreadStarting>& thread_starts() const {
    return thread_starts_;
  }

 private:
  std::vector<debug_ipc::AttachReply> attach_replies_;
  std::vector<debug_ipc::NotifyComponentStarting> component_starts_;
  std::vector<debug_ipc::NotifyProcessStarting> process_starts_;
  std::vector<debug_ipc::NotifyModules> modules_;
  std::vector<debug_ipc::NotifyException> exceptions_;
  std::vector<debug_ipc::NotifyFilterCreated> filters_;
  std::vector<debug_ipc::NotifyThreadStarting> thread_starts_;
  fit::function<void(const debug_ipc::NotifyThreadStarting&)> notify_thread_starting_callback_;
};

}  // namespace debug_agent

#endif  // SRC_DEVELOPER_DEBUG_DEBUG_AGENT_MOCK_STREAM_BACKEND_H_
